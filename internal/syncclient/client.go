// Package syncclient talks to the Recall server.
package syncclient

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/pimlabs/recall/internal/wire"
)

// Client is a Recall API client. The zero value isn't usable; use New.
type Client struct {
	BaseURL string
	Token   string
	HTTP    *http.Client
}

// New builds a client with a timeout that's generous enough for a server
// doing a semantic merge (which can take several seconds) but still bounded
// — these calls happen inside a user's session.
func New(baseURL, token string) *Client {
	return &Client{
		BaseURL: strings.TrimRight(baseURL, "/"),
		Token:   token,
		HTTP:    &http.Client{Timeout: 60 * time.Second},
	}
}

// StatusError is returned when the server answers with a non-2xx.
type StatusError struct {
	Code int
	Body string
}

func (e *StatusError) Error() string {
	return fmt.Sprintf("server returned %d: %s", e.Code, strings.TrimSpace(e.Body))
}

func (c *Client) do(ctx context.Context, req *http.Request, out any) error {
	req.Header.Set("Authorization", "Bearer "+c.Token)
	resp, err := c.HTTP.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(io.LimitReader(resp.Body, 32<<20))
	if err != nil {
		return err
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return &StatusError{Code: resp.StatusCode, Body: string(body)}
	}
	if out == nil {
		return nil
	}
	return json.Unmarshal(body, out)
}

// Push sends one memory file, or one delete.
func (c *Client) Push(ctx context.Context, r wire.PushRequest) (wire.PushResponse, error) {
	var out wire.PushResponse
	if err := r.Validate(); err != nil {
		return out, err
	}
	body, err := json.Marshal(r)
	if err != nil {
		return out, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.BaseURL+"/sync", bytes.NewReader(body))
	if err != nil {
		return out, err
	}
	req.Header.Set("Content-Type", "application/json")
	return out, c.do(ctx, req, &out)
}

// Pull fetches every file the server holds for a project, tombstones
// included — the caller needs to see those to remove local copies.
func (c *Client) Pull(ctx context.Context, projectKey string) (wire.SyncResponse, error) {
	var out wire.SyncResponse
	u := c.BaseURL + "/sync?project_key=" + url.QueryEscape(projectKey)
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, u, nil)
	if err != nil {
		return out, err
	}
	return out, c.do(ctx, req, &out)
}

// Health reads the unauthenticated health endpoint. The token is still sent
// — harmless, and it keeps one code path.
func (c *Client) Health(ctx context.Context) (wire.Health, error) {
	var out wire.Health
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.BaseURL+"/health", nil)
	if err != nil {
		return out, err
	}
	return out, c.do(ctx, req, &out)
}
