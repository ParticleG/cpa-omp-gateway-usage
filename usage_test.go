package main

import (
	"encoding/json"
	"io"
	"math"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
)

func tokenConfig(t *testing.T, endpoint string) config {
	t.Helper()
	path := filepath.Join(t.TempDir(), "token")
	if err := os.WriteFile(path, []byte("test-gateway-secret\n"), 0600); err != nil {
		t.Fatal(err)
	}
	return config{GatewayURL: endpoint, TokenFile: path}
}

func TestQuotaProjectionRemovesIdentityAndNonClaudeReports(t *testing.T) {
	t.Parallel()
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "GET" || r.URL.Path != "/v1/usage" || r.Header.Get("Authorization") != "Bearer test-gateway-secret" {
			t.Error("unexpected authenticated usage request")
		}
		io.WriteString(w, `{"generatedAt":1793010268749,"metadata":{"token":"secret-sentinel"},"reports":[{"provider":"anthropic","fetchedAt":1793010268169,"metadata":{"email":"private@example.test","accountId":"private-account"},"resetCredits":{"reason":"secret-sentinel"},"limits":[{"id":"anthropic:5h","label":"Claude 5 Hour","window":{"durationMs":18000000,"resetsAt":1793026800059},"amount":{"unit":"percent","usedFraction":0.12,"used":99,"limit":100},"metadata":{"secret":"secret-sentinel"}},{"label":"Claude Extra Usage","amount":{"unit":"USD","used":2,"limit":5}}]},{"provider":"openai-codex","limits":[{"label":"private-codex-account"}]}]}`)
	}))
	defer server.Close()
	client := newUsageClient()
	defer client.CloseIdleConnections()
	view, err := fetchUsage(t.Context(), client, tokenConfig(t, server.URL))
	if err != nil {
		t.Fatal(err)
	}
	if len(view.Reports) != 1 || len(view.Reports[0].Limits) != 1 {
		t.Fatalf("only Claude percentage windows should survive: %+v", view)
	}
	limit := view.Reports[0].Limits[0]
	if limit.UsedPercent == nil || *limit.UsedPercent != 12 || limit.RemainingPercent == nil || *limit.RemainingPercent != 88 || limit.ResetsAt != 1793026800059 || view.Reports[0].FetchedAt != 1793010268169 {
		t.Fatalf("incorrect quota projection: %+v", limit)
	}
	body, err := json.Marshal(view)
	if err != nil {
		t.Fatal(err)
	}
	for _, private := range []string{"secret-sentinel", "private@example.test", "private-account", "private-codex-account", "test-gateway-secret", "metadata", "resetCredits"} {
		if strings.Contains(string(body), private) {
			t.Fatalf("private field leaked: %s", private)
		}
	}
}

func TestQuotaUnknownZeroAndExhaustedRemainDistinct(t *testing.T) {
	t.Parallel()
	var input gatewayUsage
	if err := json.Unmarshal([]byte(`{"reports":[{"provider":"anthropic","limits":[{"amount":{"unit":"percent"}},{"amount":{"unit":"percent","used":0,"limit":100}},{"amount":{"unit":"percent","used":110,"limit":100}},{"amount":{"unit":"percent","used":0,"limit":0}}]}]}`), &input); err != nil {
		t.Fatal(err)
	}
	limits := sanitizeUsage(input).Reports[0].Limits
	if limits[0].UsedPercent != nil || limits[0].RemainingPercent != nil || limits[3].UsedPercent != nil {
		t.Fatal("missing or zero-capacity data must remain unknown")
	}
	if limits[1].UsedPercent == nil || *limits[1].UsedPercent != 0 || *limits[1].RemainingPercent != 100 {
		t.Fatal("a reported zero must remain zero with full remaining capacity")
	}
	if limits[2].UsedPercent == nil || math.Abs(*limits[2].UsedPercent-110) > 1e-9 || *limits[2].RemainingPercent != 0 {
		t.Fatal("over-limit usage must remain visible without negative remaining capacity")
	}
}

func TestQuotaRejectsRedirectsWithoutSendingToken(t *testing.T) {
	t.Parallel()
	var forwarded atomic.Int32
	destination := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { forwarded.Add(1) }))
	defer destination.Close()
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, destination.URL, http.StatusTemporaryRedirect)
	}))
	defer server.Close()
	client := newUsageClient()
	defer client.CloseIdleConnections()
	_, err := fetchUsage(t.Context(), client, tokenConfig(t, server.URL))
	if err == nil || !strings.Contains(err.Error(), "307") || forwarded.Load() != 0 {
		t.Fatalf("redirect must not forward the privileged token: err=%v, forwarded=%d", err, forwarded.Load())
	}
}

func TestQuotaErrorsNeverExposeUpstreamBodies(t *testing.T) {
	t.Parallel()
	for _, test := range []struct {
		name   string
		status int
		body   string
	}{
		{"denied", 403, `{"error":"test-gateway-secret private@example.test"}`},
		{"malformed", 200, `test-gateway-secret private@example.test`},
		{"wrong-schema", 200, `{}`},
		{"oversized", 200, strings.Repeat("x", (2<<20)+1)},
	} {
		t.Run(test.name, func(t *testing.T) {
			t.Parallel()
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(test.status); io.WriteString(w, test.body) }))
			defer server.Close()
			client := newUsageClient()
			defer client.CloseIdleConnections()
			_, err := fetchUsage(t.Context(), client, tokenConfig(t, server.URL))
			if err == nil || strings.Contains(err.Error(), "test-gateway-secret") || strings.Contains(err.Error(), "private@example.test") {
				t.Fatalf("expected a sanitized failure, got %v", err)
			}
		})
	}
}

func TestResourcePathsCannotReachQuotaFetch(t *testing.T) {
	for _, path := range []string{resourcePath + "/usage", resourcePath + "/status/usage", resourcePath + "/../usage", usagePath + "/"} {
		response := handleManagement(managementRequest{Method: "GET", Path: path})
		if response.StatusCode != http.StatusNotFound {
			t.Errorf("public or near-match path %q returned %d", path, response.StatusCode)
		}
	}
	response := handleManagement(managementRequest{Method: "POST", Path: usagePath})
	if response.StatusCode != http.StatusMethodNotAllowed {
		t.Fatalf("write method returned %d", response.StatusCode)
	}
}

func TestGatewayConfigurationRejectsEmbeddedCredentials(t *testing.T) {
	t.Parallel()
	for _, endpoint := range []string{"https://user:password@example.test", "https://example.test?token=secret", "https://example.test/v1/usage", "file:///tmp/token", "http://"} {
		if err := (config{GatewayURL: endpoint, TokenFile: "/run/secrets/token"}).validate(); err == nil {
			t.Errorf("accepted unsafe or ambiguous origin %q", endpoint)
		}
	}
}
