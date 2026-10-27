package main

import (
	"context"
	"embed"
	"encoding/json"
	"errors"
	"net/http"
	"net/url"
	"path/filepath"
	"strings"
	"sync"

	"gopkg.in/yaml.v3"
)

const (
	pluginID     = "omp-gateway-usage"
	resourcePath = "/v0/resource/plugins/" + pluginID
	usagePath    = "/v0/management/plugins/" + pluginID + "/usage"
)

// Release builds set the version from the Git tag with -ldflags.
var pluginVersion = "dev"

//go:embed web/index.html web/app.js web/style.css
var assets embed.FS

var configuration struct {
	sync.RWMutex
	value config
}

type config struct {
	GatewayURL string `yaml:"gateway_url"`
	TokenFile  string `yaml:"token_file"`
}

type managementRequest struct {
	Method string
	Path   string
}

type managementResponse struct {
	StatusCode int
	Headers    http.Header
	Body       []byte
}

type envelope struct {
	OK     bool      `json:"ok"`
	Result any       `json:"result,omitempty"`
	Error  *rpcError `json:"error,omitempty"`
}

type rpcError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

func dispatch(method string, raw []byte) []byte {
	result, err := handleMethod(method, raw)
	response := envelope{OK: err == nil, Result: result}
	if err != nil {
		response.Result = nil
		response.Error = &rpcError{Code: "plugin_error", Message: err.Error()}
	}
	body, err := json.Marshal(response)
	if err != nil {
		return []byte(`{"ok":false,"error":{"code":"encoding_error","message":"cannot encode plugin response"}}`)
	}
	return body
}

func handleMethod(method string, raw []byte) (any, error) {
	switch method {
	case "plugin.register", "plugin.reconfigure":
		var request struct {
			ConfigYAML    []byte `json:"config_yaml"`
			SchemaVersion uint32 `json:"schema_version"`
		}
		if json.Unmarshal(raw, &request) != nil {
			return nil, errors.New("invalid plugin configuration envelope")
		}
		if request.SchemaVersion < 6 {
			return nil, errors.New("plugin requires CPA RPC schema 6 or later")
		}
		cfg := config{GatewayURL: "http://omp-gateway:4000", TokenFile: "/run/secrets/omp-gateway-usage.token"}
		if yaml.Unmarshal(request.ConfigYAML, &cfg) != nil {
			return nil, errors.New("invalid plugin configuration YAML")
		}
		if err := cfg.validate(); err != nil {
			return nil, err
		}
		configuration.Lock()
		configuration.value = cfg
		configuration.Unlock()
		return map[string]any{
			"schema_version": 6,
			"metadata": map[string]any{
				"Name": "OMP Gateway Usage", "Version": pluginVersion,
				"Author":           "ParticleG",
				"GitHubRepository": "https://github.com/ParticleG/cpa-omp-gateway-usage",
				"ConfigFields": []map[string]string{
					{"Name": "gateway_url", "Type": "string", "Description": "Private OMP Gateway origin; no credentials or path."},
					{"Name": "token_file", "Type": "string", "Description": "Read-only server-side file containing the Gateway main token."},
				},
			},
			"capabilities": map[string]bool{"management_api": true},
		}, nil
	case "management.register":
		return map[string]any{
			"routes": []map[string]string{{"Method": "GET", "Path": usagePath}},
			"resources": []map[string]string{
				{"Path": "/status", "Menu": "OMP Gateway Usage", "Description": "Read-only Claude subscription quota from OMP Gateway."},
				{"Path": "/app.js"},
				{"Path": "/style.css"},
			},
		}, nil
	case "management.handle":
		var request managementRequest
		if json.Unmarshal(raw, &request) != nil {
			return nil, errors.New("invalid management request")
		}
		return handleManagement(request), nil
	default:
		return nil, errors.New("unsupported plugin method")
	}
}

func (c config) validate() error {
	u, err := url.Parse(c.GatewayURL)
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Hostname() == "" || u.User != nil || (u.Path != "" && u.Path != "/") || u.RawQuery != "" || u.ForceQuery || u.Fragment != "" {
		return errors.New("gateway_url must be an HTTP(S) origin without credentials, query or path")
	}
	if !filepath.IsAbs(c.TokenFile) {
		return errors.New("token_file must be an absolute server-side path")
	}
	return nil
}

func handleManagement(request managementRequest) managementResponse {
	if request.Method != http.MethodGet {
		response := jsonResponse(http.StatusMethodNotAllowed, map[string]string{"error": "Only GET is supported."})
		response.Headers.Set("Allow", http.MethodGet)
		return response
	}
	// Resource routes are public. Only this exact, CPA-authenticated route can read a token or contact the Gateway.
	if request.Path == usagePath {
		configuration.RLock()
		cfg := configuration.value
		configuration.RUnlock()
		report, err := fetchUsage(context.Background(), upstreamClient, cfg)
		if err != nil {
			return jsonResponse(http.StatusBadGateway, map[string]string{"error": err.Error()})
		}
		return jsonResponse(http.StatusOK, report)
	}
	var name, contentType string
	switch request.Path {
	case resourcePath + "/status":
		name, contentType = "web/index.html", "text/html; charset=utf-8"
	case resourcePath + "/app.js":
		name, contentType = "web/app.js", "text/javascript; charset=utf-8"
	case resourcePath + "/style.css":
		name, contentType = "web/style.css", "text/css; charset=utf-8"
	default:
		return jsonResponse(http.StatusNotFound, map[string]string{"error": "Not found."})
	}
	body, err := assets.ReadFile(name)
	if err != nil {
		return jsonResponse(http.StatusInternalServerError, map[string]string{"error": "Page asset unavailable."})
	}
	headers := responseHeaders(contentType)
	headers.Set("Content-Security-Policy", "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'self'; form-action 'none'")
	return managementResponse{StatusCode: http.StatusOK, Headers: headers, Body: body}
}

func jsonResponse(status int, value any) managementResponse {
	body, err := json.Marshal(value)
	if err != nil {
		status = http.StatusInternalServerError
		body = []byte(`{"error":"Cannot encode usage data."}`)
	}
	return managementResponse{StatusCode: status, Headers: responseHeaders("application/json; charset=utf-8"), Body: body}
}

func responseHeaders(contentType string) http.Header {
	return http.Header{
		"Content-Type":           {contentType},
		"Cache-Control":          {"no-store"},
		"X-Content-Type-Options": {"nosniff"},
		"Referrer-Policy":        {"no-referrer"},
	}
}

func usageURL(cfg config) string {
	return strings.TrimRight(cfg.GatewayURL, "/") + "/v1/usage"
}
