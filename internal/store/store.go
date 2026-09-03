// Package store is the server's SQLite persistence.
//
// The schema here is deliberately identical to the one the Node server
// created, because the Go server opens the *existing production database
// file* rather than migrating to a new one. That is what makes the cutover
// reversible: roll back by starting the old container against the same
// untouched file.
package store

import (
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/pimlabs/recall/internal/wire"

	_ "modernc.org/sqlite"
)

// TimeFormat matches JavaScript's Date.toISOString() — millisecond
// precision with a Z suffix. Timestamps written by the Node server are
// already in the database in this shape, and clients and the admin page
// display them, so the format is part of the compatibility surface.
const TimeFormat = "2006-01-02T15:04:05.000Z"

// Now returns a timestamp in the stored format.
func Now() string { return time.Now().UTC().Format(TimeFormat) }

type Store struct {
	db *sql.DB
}

// Open opens (creating if needed) the database at path.
func Open(path string) (*Store, error) {
	if dir := filepath.Dir(path); dir != "" {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return nil, err
		}
	}
	db, err := sql.Open("sqlite", path)
	if err != nil {
		return nil, err
	}
	s := &Store{db: db}
	if err := s.migrate(); err != nil {
		db.Close()
		return nil, err
	}
	return s, nil
}

func (s *Store) Close() error { return s.db.Close() }

func (s *Store) migrate() error {
	_, err := s.db.Exec(`
		CREATE TABLE IF NOT EXISTS memory_files (
			project_key TEXT NOT NULL,
			file_path   TEXT NOT NULL,
			content     TEXT NOT NULL,
			source_env  TEXT,
			updated_at  TEXT NOT NULL,
			deleted     INTEGER NOT NULL DEFAULT 0,
			PRIMARY KEY (project_key, file_path)
		);
	`)
	if err != nil {
		return err
	}

	// Databases created before tombstones existed have no `deleted` column.
	// Adding it is safe and idempotent when guarded like this.
	rows, err := s.db.Query(`PRAGMA table_info(memory_files)`)
	if err != nil {
		return err
	}
	defer rows.Close()

	hasDeleted := false
	for rows.Next() {
		var (
			cid         int
			name, ctype string
			notnull, pk int
			dflt        sql.NullString
		)
		if err := rows.Scan(&cid, &name, &ctype, &notnull, &dflt, &pk); err != nil {
			return err
		}
		if name == "deleted" {
			hasDeleted = true
		}
	}
	if err := rows.Err(); err != nil {
		return err
	}
	if !hasDeleted {
		if _, err := s.db.Exec(`ALTER TABLE memory_files ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0`); err != nil {
			return err
		}
	}
	return nil
}

// Existing is the stored state of one file, used to decide whether a push
// needs merging.
type Existing struct {
	Content string
	Deleted bool
	Found   bool
}

// Get reads one row.
func (s *Store) Get(projectKey, filePath string) (Existing, error) {
	var (
		content string
		deleted int
	)
	err := s.db.QueryRow(
		`SELECT content, deleted FROM memory_files WHERE project_key = ? AND file_path = ?`,
		projectKey, filePath,
	).Scan(&content, &deleted)
	if err == sql.ErrNoRows {
		return Existing{}, nil
	}
	if err != nil {
		return Existing{}, err
	}
	return Existing{Content: content, Deleted: deleted != 0, Found: true}, nil
}

// Upsert writes content, clearing any tombstone.
func (s *Store) Upsert(projectKey, filePath, content, sourceEnv, updatedAt string) error {
	_, err := s.db.Exec(`
		INSERT INTO memory_files (project_key, file_path, content, source_env, updated_at, deleted)
		VALUES (?, ?, ?, ?, ?, 0)
		ON CONFLICT(project_key, file_path) DO UPDATE SET
			content = excluded.content,
			source_env = excluded.source_env,
			updated_at = excluded.updated_at,
			deleted = 0
	`, projectKey, filePath, content, nullable(sourceEnv), updatedAt)
	return err
}

// Tombstone marks a file deleted while deliberately leaving its content in
// place: a mistaken delete stays recoverable at the database level, even
// though nothing in the app surfaces an undo yet. GET /sync withholds the
// content so a pull can't resurrect it.
func (s *Store) Tombstone(projectKey, filePath, sourceEnv, updatedAt string) error {
	_, err := s.db.Exec(`
		INSERT INTO memory_files (project_key, file_path, content, source_env, updated_at, deleted)
		VALUES (?, ?, '', ?, ?, 1)
		ON CONFLICT(project_key, file_path) DO UPDATE SET
			source_env = excluded.source_env,
			updated_at = excluded.updated_at,
			deleted = 1
	`, projectKey, filePath, nullable(sourceEnv), updatedAt)
	return err
}

// List returns every file for a project, tombstones included so a pulling
// client knows what to remove locally.
func (s *Store) List(projectKey string) ([]wire.File, error) {
	rows, err := s.db.Query(`
		SELECT file_path, content, COALESCE(source_env, ''), updated_at, deleted
		FROM memory_files WHERE project_key = ? ORDER BY file_path
	`, projectKey)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	files := []wire.File{}
	for rows.Next() {
		var (
			f       wire.File
			content string
			deleted int
		)
		if err := rows.Scan(&f.FilePath, &content, &f.SourceEnv, &f.UpdatedAt, &deleted); err != nil {
			return nil, err
		}
		f.Deleted = deleted != 0
		if !f.Deleted {
			c := content
			f.Content = &c
		}
		files = append(files, f)
	}
	return files, rows.Err()
}

// LastSyncAt is the most recent write across all projects, for /health.
func (s *Store) LastSyncAt() (string, error) {
	var v sql.NullString
	if err := s.db.QueryRow(`SELECT MAX(updated_at) FROM memory_files`).Scan(&v); err != nil {
		return "", err
	}
	return v.String, nil
}

// AdminStats aggregates per project for the admin page.
func (s *Store) AdminStats() ([]wire.ProjectStats, wire.AdminTotals, error) {
	rows, err := s.db.Query(`
		SELECT project_key,
		       SUM(CASE WHEN deleted = 0 THEN 1 ELSE 0 END),
		       SUM(CASE WHEN deleted = 1 THEN 1 ELSE 0 END),
		       MAX(updated_at)
		FROM memory_files GROUP BY project_key ORDER BY MAX(updated_at) DESC
	`)
	if err != nil {
		return nil, wire.AdminTotals{}, err
	}
	defer rows.Close()

	projects := []wire.ProjectStats{}
	totals := wire.AdminTotals{}
	for rows.Next() {
		var p wire.ProjectStats
		if err := rows.Scan(&p.ProjectKey, &p.FileCount, &p.DeletedCount, &p.LastUpdatedAt); err != nil {
			return nil, wire.AdminTotals{}, err
		}
		p.Sources = []string{}
		projects = append(projects, p)
		totals.FileCount += p.FileCount
		totals.DeletedCount += p.DeletedCount
	}
	if err := rows.Err(); err != nil {
		return nil, wire.AdminTotals{}, err
	}
	totals.ProjectCount = len(projects)

	// Sources are collected separately rather than with GROUP_CONCAT:
	// SQLite won't take a custom separator together with DISTINCT, and
	// source_env is client-supplied, so a value containing a comma would
	// silently split into bogus entries.
	srcRows, err := s.db.Query(`SELECT DISTINCT project_key, source_env FROM memory_files WHERE source_env IS NOT NULL`)
	if err != nil {
		return nil, wire.AdminTotals{}, err
	}
	defer srcRows.Close()

	byProject := map[string][]string{}
	for srcRows.Next() {
		var key, src string
		if err := srcRows.Scan(&key, &src); err != nil {
			return nil, wire.AdminTotals{}, err
		}
		if src != "" {
			byProject[key] = append(byProject[key], src)
		}
	}
	if err := srcRows.Err(); err != nil {
		return nil, wire.AdminTotals{}, err
	}
	for i := range projects {
		if s := byProject[projects[i].ProjectKey]; len(s) > 0 {
			sort.Strings(s)
			projects[i].Sources = s
		}
	}
	return projects, totals, nil
}

// Backup writes a consistent snapshot via VACUUM INTO, which is safe
// against a live database — unlike copying the file — then prunes the
// oldest snapshots beyond keep.
func (s *Store) Backup(dir string, keep int) (string, error) {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", err
	}
	// Mirrors the Node server's naming, which was
	// toISOString().replace(/[:.]/g, "-") — millisecond precision matters
	// twice over: without it two snapshots in the same second collide (and
	// VACUUM INTO refuses to overwrite), and the prune below sorts these
	// names lexicographically, alongside any snapshots the Node server
	// already wrote into the same directory.
	//
	// Note the fractional seconds must be written as ".000" in the layout —
	// "-000" would render three literal zeroes rather than milliseconds.
	stamp := strings.NewReplacer(":", "-", ".", "-").Replace(time.Now().UTC().Format(TimeFormat))
	dest := filepath.Join(dir, fmt.Sprintf("recall-%s.db", stamp))

	if _, err := s.db.Exec(`VACUUM INTO ?`, dest); err != nil {
		return "", err
	}

	entries, err := os.ReadDir(dir)
	if err != nil {
		return dest, err
	}
	var snapshots []string
	for _, e := range entries {
		n := e.Name()
		if len(n) > 7 && n[:7] == "recall-" && filepath.Ext(n) == ".db" {
			snapshots = append(snapshots, n)
		}
	}
	sort.Strings(snapshots)
	for i := 0; i < len(snapshots)-keep; i++ {
		os.Remove(filepath.Join(dir, snapshots[i]))
	}
	return dest, nil
}

func nullable(s string) any {
	if s == "" {
		return nil
	}
	return s
}
