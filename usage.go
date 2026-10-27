package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"strings"
	"time"
)

var upstreamClient = newUsageClient()

func newUsageClient() *http.Client {
	transport := http.DefaultTransport.(*http.Transport).Clone()
	// The Gateway token must stay on the configured private connection, not an environment-selected proxy.
	transport.Proxy = nil
	return &http.Client{
		Transport:     transport,
		Timeout:       12 * time.Second,
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
}

type gatewayUsage struct {
	GeneratedAt int64           `json:"generatedAt"`
	Reports     []gatewayReport `json:"reports"`
}

type gatewayReport struct {
	Provider  string         `json:"provider"`
	FetchedAt int64          `json:"fetchedAt"`
	Limits    []gatewayLimit `json:"limits"`
}

type gatewayLimit struct {
	Label  string `json:"label"`
	Window struct {
		DurationMS int64 `json:"durationMs"`
		ResetsAt   int64 `json:"resetsAt"`
	} `json:"window"`
	Amount struct {
		Unit         string   `json:"unit"`
		Used         *float64 `json:"used"`
		Limit        *float64 `json:"limit"`
		UsedFraction *float64 `json:"usedFraction"`
	} `json:"amount"`
}

type usageView struct {
	GeneratedAt int64        `json:"generatedAt"`
	Reports     []reportView `json:"reports"`
}

type reportView struct {
	Provider  string      `json:"provider"`
	FetchedAt int64       `json:"fetchedAt"`
	Limits    []limitView `json:"limits"`
}

type limitView struct {
	Label            string   `json:"label"`
	UsedPercent      *float64 `json:"usedPercent"`
	RemainingPercent *float64 `json:"remainingPercent"`
	ResetsAt         int64    `json:"resetsAt"`
	DurationMS       int64    `json:"durationMs"`
}

func fetchUsage(ctx context.Context, client *http.Client, cfg config) (usageView, error) {
	var empty usageView
	tokenBytes, err := os.ReadFile(cfg.TokenFile)
	if err != nil {
		return empty, errors.New("Gateway token file is unavailable; check the server-side mount.")
	}
	token := strings.TrimSpace(string(tokenBytes))
	if token == "" || strings.ContainsAny(token, "\r\n") {
		return empty, errors.New("Gateway token file is invalid.")
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, usageURL(cfg), nil)
	if err != nil {
		return empty, errors.New("Gateway configuration is invalid.")
	}
	request.Header.Set("Authorization", "Bearer "+token)
	request.Header.Set("Accept", "application/json")
	response, err := client.Do(request)
	if err != nil {
		return empty, errors.New("Gateway quota request failed or timed out.")
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return empty, fmt.Errorf("Gateway quota request returned HTTP %d; check Gateway health and its main token.", response.StatusCode)
	}
	const maxBody = 2 << 20
	body, err := io.ReadAll(io.LimitReader(response.Body, maxBody+1))
	if err != nil || len(body) > maxBody {
		return empty, errors.New("Gateway quota response is unreadable or too large.")
	}
	var upstream gatewayUsage
	if json.Unmarshal(body, &upstream) != nil || upstream.GeneratedAt <= 0 || upstream.Reports == nil {
		return empty, errors.New("Gateway returned an unsupported quota response.")
	}
	return sanitizeUsage(upstream), nil
}

func sanitizeUsage(upstream gatewayUsage) usageView {
	result := usageView{GeneratedAt: upstream.GeneratedAt, Reports: make([]reportView, 0, len(upstream.Reports))}
	for _, report := range upstream.Reports {
		if report.Provider != "anthropic" {
			continue
		}
		item := reportView{Provider: "anthropic", FetchedAt: report.FetchedAt, Limits: make([]limitView, 0, len(report.Limits))}
		for _, limit := range report.Limits {
			if limit.Amount.Unit != "percent" {
				continue
			}
			view := limitView{Label: limit.Label, ResetsAt: limit.Window.ResetsAt, DurationMS: limit.Window.DurationMS}
			var used float64
			known := false
			if limit.Amount.UsedFraction != nil {
				used, known = *limit.Amount.UsedFraction*100, true
			} else if limit.Amount.Used != nil && limit.Amount.Limit != nil && *limit.Amount.Limit > 0 {
				used, known = *limit.Amount.Used / *limit.Amount.Limit * 100, true
			}
			if known && used >= 0 {
				remaining := max(0, 100-used)
				view.UsedPercent, view.RemainingPercent = &used, &remaining
			}
			item.Limits = append(item.Limits, view)
		}
		result.Reports = append(result.Reports, item)
	}
	return result
}
