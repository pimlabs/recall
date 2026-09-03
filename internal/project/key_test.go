package project

import "testing"

// These keys are load-bearing for data continuity: a project's synced
// history lives under its key on the server, so any change here orphans it.
// The first three rows are the exact forms verified live in
// docs/phase-0-findings.md and must keep producing what the shell
// implementation produced today.
func TestKeyFromRemote(t *testing.T) {
	tests := []struct {
		name string
		in   string
		want string
	}{
		{
			name: "proxied remote a cloud sandbox rewrites origin to",
			in:   "http://local_proxy@127.0.0.1:41729/git/pimlabs/recall",
			want: "pimlabs/recall",
		},
		{
			name: "ssh form",
			in:   "git@github.com:pimlabs/recall.git",
			want: "pimlabs/recall",
		},
		{
			name: "https form",
			in:   "https://github.com/pimlabs/recall.git",
			want: "pimlabs/recall",
		},
		{
			name: "the three forms above must all agree — that is the point",
			in:   "ssh://git@github.com/pimlabs/recall",
			want: "pimlabs/recall",
		},
		{
			name: "port number must not be mistaken for a path segment",
			in:   "ssh://git@github.com:22/pimlabs/recall.git",
			want: "pimlabs/recall",
		},
		{
			name: "trailing slash",
			in:   "https://github.com/pimlabs/recall/",
			want: "pimlabs/recall",
		},
		{
			name: "trailing slash after .git",
			in:   "https://github.com/pimlabs/recall.git/",
			want: "pimlabs/recall",
		},
		{
			name: "case is normalized so two clones can't disagree",
			in:   "git@github.com:PimLabs/Recall.git",
			want: "pimlabs/recall",
		},
		{
			name: "surrounding whitespace from `git remote get-url` output",
			in:   "  git@github.com:pimlabs/recall.git\n",
			want: "pimlabs/recall",
		},
		{
			name: "nested groups collapse to the last two segments — a known, documented limitation",
			in:   "https://gitlab.com/some-group/sub-group/repo.git",
			want: "sub-group/repo",
		},
		{
			name: "empty input derives nothing",
			in:   "",
			want: "",
		},
		{
			name: "single segment derives nothing",
			in:   "repo",
			want: "",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := KeyFromRemote(tt.in); got != tt.want {
				t.Errorf("KeyFromRemote(%q) = %q, want %q", tt.in, got, tt.want)
			}
		})
	}
}

func TestKeyFallsBackToLocalWithoutRemote(t *testing.T) {
	got := Key("", "/home/user/scratch")
	want := "local:-home-user-scratch"
	if got != want {
		t.Errorf("Key(no remote) = %q, want %q", got, want)
	}
}

func TestKeyPrefersRemoteOverLocal(t *testing.T) {
	got := Key("git@github.com:pimlabs/recall.git", "/anywhere/at/all")
	want := "pimlabs/recall"
	if got != want {
		t.Errorf("Key() = %q, want %q — the whole point is the path must not affect it", got, want)
	}
}

// Two machines with the same repo checked out at different paths, and
// different remote URL shapes, must land on the same key — otherwise sync
// silently splits into two histories.
func TestKeyAgreesAcrossMachines(t *testing.T) {
	laptop := Key("git@github.com:pimlabs/recall.git", "/Users/eko/code/recall")
	cloud := Key("http://local_proxy@127.0.0.1:9999/git/pimlabs/recall", "/home/user/recall")
	if laptop != cloud {
		t.Errorf("laptop key %q != cloud key %q", laptop, cloud)
	}
}
