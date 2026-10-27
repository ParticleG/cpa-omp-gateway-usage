(() => {
  "use strict";

  const usagePath = "/v0/management/plugins/omp-gateway-usage/usage";
  const authStorage = "cli-proxy-auth";
  const reports = document.querySelector("#reports");
  const notice = document.querySelector("#notice");
  const updated = document.querySelector("#updated");
  const refresh = document.querySelector("#refresh");
  const number = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });
  const date = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
  const timezone = date.resolvedOptions().timeZone;
  let pending = null;

  // CPAMC secureStorage v1 uses reversible obfuscation, not encryption.
  // Match the existing Key Model Access session integration without storing another copy.
  function storedValue(name) {
    try {
      let raw = localStorage.getItem(name);
      if (raw === null) return null;
      if (raw.startsWith("enc::v1::")) {
        const bytes = Uint8Array.from(atob(raw.slice(9)), (character) => character.charCodeAt(0));
        const key = new TextEncoder().encode(`cli-proxy-api-webui::secure-storage|${location.host}|${navigator.userAgent}`);
        for (let index = 0; index < bytes.length; index++) bytes[index] ^= key[index % key.length];
        raw = new TextDecoder().decode(bytes);
      }
      try { return JSON.parse(raw); } catch { return raw; }
    } catch {
      return null;
    }
  }

  function managementKey() {
    try {
      if (localStorage.getItem("isLoggedIn") !== "true") return "";
      const key = storedValue(authStorage)?.state?.managementKey || storedValue("managementKey");
      return typeof key === "string" ? key.trim() : "";
    } catch {
      return "";
    }
  }

  function syncTheme() {
    const theme = storedValue("cli-proxy-theme")?.state?.theme;
    document.documentElement.dataset.theme = theme === "white" ? "light" : theme || "auto";
  }

  function message(text, kind = "info") {
    notice.textContent = text;
    notice.dataset.kind = kind;
  }

  function clearReport() {
    reports.replaceChildren();
    updated.textContent = "No quota data loaded.";
  }

  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function timestamp(value) {
    return Number.isFinite(value) && value > 0 ? `${date.format(value)} (${timezone})` : "Not reported";
  }

  function render(payload) {
    reports.replaceChildren();
    updated.textContent = `Gateway snapshot: ${timestamp(payload.generatedAt)}`;
    if (!payload.reports.length) {
      message("No Claude subscription report is currently available from the Gateway.");
      return;
    }
    let count = 0;
    for (const [index, report] of payload.reports.entries()) {
      const section = element("section", "account");
      section.append(element("h2", "", payload.reports.length === 1 ? "Claude subscription" : `Claude subscription ${index + 1}`));
      section.append(element("p", "fetched", `Upstream data fetched: ${timestamp(report.fetchedAt)}`));
      const cards = element("div", "cards");
      for (const limit of report.limits) {
        count++;
        const card = element("article", "card");
        card.append(element("h3", "", limit.label || "Subscription limit"));
        const known = Number.isFinite(limit.usedPercent);
        card.dataset.level = !known ? "unknown" : limit.usedPercent >= 100 ? "exhausted" : limit.usedPercent >= 80 ? "warning" : "ok";
        card.append(element("p", "percent", known ? `${number.format(limit.usedPercent)}% used` : "Unavailable"));
        card.append(element("p", "remaining", Number.isFinite(limit.remainingPercent) ? `${number.format(limit.remainingPercent)}% remaining` : "Usage was not reported by the provider."));
        if (known) {
          const progress = document.createElement("progress");
          progress.max = 100;
          progress.value = Math.min(100, Math.max(0, limit.usedPercent));
          progress.setAttribute("aria-label", `${limit.label || "Subscription limit"}: ${number.format(limit.usedPercent)} percent used`);
          card.append(progress);
        }
        card.append(element("p", "reset", `Resets: ${timestamp(limit.resetsAt)}`));
        if (limit.resetsAt > 0 && limit.resetsAt <= Date.now()) {
          card.append(element("p", "quota-state", "The reported reset time has passed. Refresh to check the next upstream snapshot."));
        }
        cards.append(card);
      }
      if (!report.limits.length) section.append(element("p", "", "No subscription percentage windows were reported for this account."));
      section.append(cards);
      reports.append(section);
    }
    message(count ? "Showing account-wide subscription quota. Refresh is manual and does not bypass the Gateway cache." : "Quota data is unavailable; this does not mean that the account has zero usage.");
  }

  async function loadUsage() {
    pending?.abort();
    pending = null;
    clearReport();
    const key = managementKey();
    if (!key) {
      refresh.disabled = false;
      reports.setAttribute("aria-busy", "false");
      message("Open Management Center, sign in with Remember password enabled, then return here and click Refresh. This page does not save a separate management key.");
      return;
    }
    const controller = new AbortController();
    pending = controller;
    const timer = setTimeout(() => controller.abort(), 15000);
    refresh.disabled = true;
    reports.setAttribute("aria-busy", "true");
    message("Reading the Gateway quota snapshot…");
    try {
      const response = await fetch(usagePath, {
        method: "GET",
        headers: { Accept: "application/json", Authorization: `Bearer ${key}` },
        cache: "no-store",
        credentials: "same-origin",
        redirect: "error",
        signal: controller.signal
      });
      if (response.status === 401 || response.status === 403) throw new Error("CPA management access was denied. Sign in again in Management Center.");
      const payload = await response.json();
      if (!response.ok) throw new Error(typeof payload.error === "string" ? payload.error : `Quota request failed (HTTP ${response.status}).`);
      if (!Array.isArray(payload.reports)) throw new Error("The quota response format is unsupported.");
      // Never repaint an old privileged response after logout or a different login.
      if (pending === controller && managementKey() === key) render(payload);
    } catch (error) {
      if (pending !== controller) return;
      clearReport();
      message(error.name === "AbortError" ? "Quota request timed out. Try Refresh again." : error.message, "error");
    } finally {
      clearTimeout(timer);
      if (pending === controller) {
        pending = null;
        refresh.disabled = false;
        reports.setAttribute("aria-busy", "false");
      }
    }
  }

  refresh.addEventListener("click", loadUsage);
  window.addEventListener("storage", (event) => {
    if (event.key === "cli-proxy-theme" || event.key === null) syncTheme();
    if ([authStorage, "managementKey", "isLoggedIn", null].includes(event.key)) loadUsage();
  });
  window.addEventListener("pagehide", () => pending?.abort());
  syncTheme();
  loadUsage();
})();
