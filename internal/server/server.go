// Package server is the sync API — the Go replacement for server/index.js.
//
// The routes, status codes, JSON shapes and auth behavior are deliberately
// identical to that implementation, because during migration a machine
// still on the shell hooks and one already on this binary talk to the same
// deployment.
package server

import (
	"context"
	"crypto/subtle"
	_ "embed"
	"encoding/json"
	"errors"
	"io"
	"log"
	"net"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/pimlabs/recall/internal/config"
	"github.com/pimlabs/recall/internal/merge"
	"github.com/pimlabs/recall/internal/store"
	"github.com/pimlabs/recall/internal/wire"
)

//go:embed admin.html
var adminHTML string

// maxBodyBytes bounds a single push. Memory files are prose; anything this
// large is a bug or an attack, not a note.
const maxBodyBytes = 5 << 20

type Server struct {
	cfg    config.Server
	store  *store.Store
	merger merge.Merger

	startedAt string

	mu             sync.RWMutex
	lastBackupAt   string
	lastMergeAt    string
	lastMergeError *wire.MergeError
	claudeStatus   merge.Status

	limiter *rateLimiter
}

// New builds a server around an already-open store.
func New(cfg config.Server, st *store.Store) *Server {
	return &Server{
		cfg:       cfg,
		store:     st,
		merger:    merge.Merger{Bin: cfg.ClaudeBin, Timeout: cfg.MergeTimeout},
		startedAt: store.Now(),
		limiter:   newRateLimiter(cfg.RateLimitWindow, cfg.RateLimitMax),
	}
}

// Handler builds the mux. /health and /admin are registered ahead of the
// auth and rate-limit checks: /health must stay pollable by uptime tooling
// without a token, and /admin is static markup that carries no data — the
// page asks the viewer for a token and fetches /admin/stats itself.
func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", s.handleHealth)
	mux.HandleFunc("/admin", s.handleAdminPage)
	mux.HandleFunc("/sync", s.guard(s.handleSync))
	mux.HandleFunc("/admin/stats", s.guard(s.handleAdminStats))
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, http.StatusNotFound, wire.ErrorResponse{Error: "not found"})
	})
	return mux
}

// Start runs background work: the first Claude CLI status check, its
// refresh loop, and backups. All of it is best-effort — none of it may take
// the sync API down.
func (s *Server) Start(ctx context.Context) {
	if s.cfg.MergeEnabled {
		s.refreshClaudeStatus(ctx)
		go s.loop(ctx, s.cfg.ClaudeStatusInterval, func() { s.refreshClaudeStatus(ctx) })
	}
	if s.cfg.BackupDir != "" {
		s.runBackup()
		go s.loop(ctx, s.cfg.BackupInterval, s.runBackup)
	}
}

func (s *Server) loop(ctx context.Context, every time.Duration, fn func()) {
	t := time.NewTicker(every)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			fn()
		}
	}
}

func (s *Server) runBackup() {
	dest, err := s.store.Backup(s.cfg.BackupDir, s.cfg.BackupKeep)
	if err != nil {
		// A failed backup must never stop the server; it becomes visible
		// through /health's last_backup_at going stale.
		log.Printf("backup failed: %v", err)
		return
	}
	s.mu.Lock()
	s.lastBackupAt = store.Now()
	s.mu.Unlock()
	log.Printf("backup written: %s", dest)
}

func (s *Server) refreshClaudeStatus(ctx context.Context) {
	st := s.merger.CheckStatus(ctx)
	s.mu.Lock()
	s.claudeStatus = st
	s.mu.Unlock()
}

// guard applies rate limiting and then auth, in that order, so a flood of
// invalid tokens is limited too rather than escaping the limiter by never
// reaching the auth check.
func (s *Server) guard(next http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if s.limiter.limited(clientIP(r)) {
			w.Header().Set("Retry-After", strconv.Itoa(int(s.cfg.RateLimitWindow.Seconds())))
			writeJSON(w, http.StatusTooManyRequests, wire.ErrorResponse{Error: "rate limit exceeded, try again later"})
			return
		}
		if !s.authorized(r) {
			writeJSON(w, http.StatusUnauthorized, wire.ErrorResponse{Error: "unauthorized"})
			return
		}
		next(w, r)
	}
}

func (s *Server) authorized(r *http.Request) bool {
	scheme, value, ok := strings.Cut(r.Header.Get("Authorization"), " ")
	if !ok || scheme != "Bearer" || value == "" {
		return false
	}
	return subtle.ConstantTimeCompare([]byte(value), []byte(s.cfg.Token)) == 1
}

func (s *Server) handleSync(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodPost:
		s.handlePush(w, r)
	case http.MethodGet:
		s.handlePull(w, r)
	default:
		writeJSON(w, http.StatusNotFound, wire.ErrorResponse{Error: "not found"})
	}
}

func (s *Server) handlePush(w http.ResponseWriter, r *http.Request) {
	body, err := io.ReadAll(io.LimitReader(r.Body, maxBodyBytes))
	if err != nil {
		writeJSON(w, http.StatusBadRequest, wire.ErrorResponse{Error: "could not read body"})
		return
	}
	var req wire.PushRequest
	if err := json.Unmarshal(body, &req); err != nil {
		writeJSON(w, http.StatusBadRequest, wire.ErrorResponse{Error: "invalid json body"})
		return
	}
	if req.ProjectKey == "" || req.FilePath == "" {
		writeJSON(w, http.StatusBadRequest, wire.ErrorResponse{
			Error: "project_key, file_path, and content (string) are required, unless deleted is true",
		})
		return
	}
	if err := wire.ValidateFilePath(req.FilePath); err != nil {
		writeJSON(w, http.StatusBadRequest, wire.ErrorResponse{Error: "file_path must be relative, no traversal"})
		return
	}

	updatedAt := store.Now()

	if req.Deleted {
		if err := s.store.Tombstone(req.ProjectKey, req.FilePath, req.SourceEnv, updatedAt); err != nil {
			writeJSON(w, http.StatusInternalServerError, wire.ErrorResponse{Error: err.Error()})
			return
		}
		writeJSON(w, http.StatusOK, wire.PushResponse{
			OK: true, ProjectKey: req.ProjectKey, FilePath: req.FilePath,
			Deleted: true, UpdatedAt: updatedAt,
		})
		return
	}

	existing, err := s.store.Get(req.ProjectKey, req.FilePath)
	if err != nil {
		writeJSON(w, http.StatusInternalServerError, wire.ErrorResponse{Error: err.Error()})
		return
	}

	content := req.Content
	merged := false

	// Merge only when there is genuinely something to reconcile. A brand-new
	// file, a revived tombstone (the delete already expressed intent to
	// discard the old content), or an unchanged re-push all skip straight to
	// a write — cheaper, and it keeps a merge from ever second-guessing
	// content that didn't actually conflict.
	if s.shouldMerge(existing, req.Content) {
		out, err := s.merger.Merge(r.Context(), existing.Content, req.Content)
		if err != nil {
			log.Printf("merge failed for %s/%s, falling back to last-write-wins: %v", req.ProjectKey, req.FilePath, err)
			s.mu.Lock()
			s.lastMergeError = &wire.MergeError{Message: err.Error(), At: store.Now()}
			s.mu.Unlock()
		} else {
			content = out
			merged = true
			s.mu.Lock()
			s.lastMergeAt = store.Now()
			s.lastMergeError = nil
			s.mu.Unlock()
		}
	}

	if err := s.store.Upsert(req.ProjectKey, req.FilePath, content, req.SourceEnv, updatedAt); err != nil {
		writeJSON(w, http.StatusInternalServerError, wire.ErrorResponse{Error: err.Error()})
		return
	}
	writeJSON(w, http.StatusOK, wire.PushResponse{
		OK: true, ProjectKey: req.ProjectKey, FilePath: req.FilePath,
		Deleted: false, Merged: merged, UpdatedAt: updatedAt,
	})
}

func (s *Server) shouldMerge(existing store.Existing, incoming string) bool {
	if !s.cfg.MergeEnabled || !existing.Found || existing.Deleted {
		return false
	}
	if existing.Content == incoming {
		return false
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	// Don't even attempt it when the CLI isn't logged in: every attempt
	// would burn a subprocess and a timeout before failing to the same
	// place.
	return s.claudeStatus.LoggedIn
}

func (s *Server) handlePull(w http.ResponseWriter, r *http.Request) {
	projectKey := r.URL.Query().Get("project_key")
	if projectKey == "" {
		writeJSON(w, http.StatusBadRequest, wire.ErrorResponse{Error: "project_key query param is required"})
		return
	}
	files, err := s.store.List(projectKey)
	if err != nil {
		writeJSON(w, http.StatusInternalServerError, wire.ErrorResponse{Error: err.Error()})
		return
	}
	writeJSON(w, http.StatusOK, wire.SyncResponse{ProjectKey: projectKey, Files: files})
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	lastSync, err := s.store.LastSyncAt()
	if err != nil {
		writeJSON(w, http.StatusInternalServerError, wire.ErrorResponse{Error: err.Error()})
		return
	}

	s.mu.RLock()
	cs := s.claudeStatus
	h := wire.Health{
		Status:       "ok",
		GitCommit:    s.cfg.GitCommit,
		StartedAt:    s.startedAt,
		LastSyncAt:   lastSync,
		LastBackupAt: s.lastBackupAt,
		Merge: wire.MergeStatus{
			Enabled:        s.cfg.MergeEnabled,
			LastMergeAt:    s.lastMergeAt,
			LastMergeError: s.lastMergeError,
		},
	}
	s.mu.RUnlock()

	if cs.CheckedAt != "" {
		available, loggedIn := cs.Available, cs.LoggedIn
		h.Merge.ClaudeCLI = wire.ClaudeCLIStatus{
			CheckedAt: cs.CheckedAt,
			Available: &available,
			LoggedIn:  &loggedIn,
			Error:     cs.Err,
		}
	}
	writeJSON(w, http.StatusOK, h)
}

func (s *Server) handleAdminPage(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("X-Content-Type-Options", "nosniff")
	// The token the page holds lives in sessionStorage on this origin;
	// default-src 'none' with connect-src 'self' means even a future
	// injection bug here would have nowhere to send it.
	w.Header().Set("Content-Security-Policy",
		"default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'")
	io.WriteString(w, adminHTML)
}

func (s *Server) handleAdminStats(w http.ResponseWriter, r *http.Request) {
	projects, totals, err := s.store.AdminStats()
	if err != nil {
		writeJSON(w, http.StatusInternalServerError, wire.ErrorResponse{Error: err.Error()})
		return
	}
	s.mu.RLock()
	lastBackup := s.lastBackupAt
	s.mu.RUnlock()

	writeJSON(w, http.StatusOK, wire.AdminStats{
		Projects: projects, Totals: totals,
		GitCommit: s.cfg.GitCommit, LastBackupAt: lastBackup,
	})
}

// ListenAndServe runs until ctx is cancelled, then shuts down gracefully so
// an in-flight merge isn't cut off mid-write.
func (s *Server) ListenAndServe(ctx context.Context) error {
	srv := &http.Server{
		Addr:              s.cfg.Addr,
		Handler:           s.Handler(),
		ReadHeaderTimeout: 10 * time.Second,
	}
	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
		defer cancel()
		srv.Shutdown(shutdownCtx)
	}()

	log.Printf("recall server listening on %s (db: %s)", s.cfg.Addr, s.cfg.DBPath)
	if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
		return err
	}
	return nil
}

func writeJSON(w http.ResponseWriter, status int, body any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(body)
}

// clientIP trusts Cloudflare's header because every request that reaches
// this process arrives through the tunnel — the origin port is never
// published (compose uses `expose`, not `ports`), so the header can't be
// spoofed by hitting the origin directly.
func clientIP(r *http.Request) string {
	if ip := r.Header.Get("CF-Connecting-IP"); ip != "" {
		return ip
	}
	if xff := r.Header.Get("X-Forwarded-For"); xff != "" {
		first, _, _ := strings.Cut(xff, ",")
		return strings.TrimSpace(first)
	}
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return r.RemoteAddr
	}
	return host
}

// rateLimiter is a per-IP fixed window, in memory — enough for a
// single-owner server, and it needs no external store.
type rateLimiter struct {
	window time.Duration
	max    int

	mu      sync.Mutex
	buckets map[string]*bucket
}

type bucket struct {
	count       int
	windowStart time.Time
}

func newRateLimiter(window time.Duration, max int) *rateLimiter {
	rl := &rateLimiter{window: window, max: max, buckets: map[string]*bucket{}}
	go rl.sweep()
	return rl
}

func (rl *rateLimiter) limited(ip string) bool {
	rl.mu.Lock()
	defer rl.mu.Unlock()

	now := time.Now()
	b, ok := rl.buckets[ip]
	if !ok || now.Sub(b.windowStart) >= rl.window {
		b = &bucket{windowStart: now}
		rl.buckets[ip] = b
	}
	b.count++
	return b.count > rl.max
}

// sweep drops stale buckets; without it every IP that ever connected would
// stay in memory for the life of the process.
func (rl *rateLimiter) sweep() {
	for range time.Tick(rl.window) {
		rl.mu.Lock()
		now := time.Now()
		for ip, b := range rl.buckets {
			if now.Sub(b.windowStart) >= 2*rl.window {
				delete(rl.buckets, ip)
			}
		}
		rl.mu.Unlock()
	}
}
