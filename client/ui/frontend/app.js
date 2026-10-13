// PingPath's window: talks to the Rust side (src/main.rs) through
// window.__TAURI__ and shows what the PingPath service reports.
//
// - Commands: games, status, measure, enable, disable, dismiss, settings,
//   set_settings, copy_report, get_language, set_language, plus a few for
//   the window itself (app_info, connection, reconnect, acknowledged_games,
//   acknowledge_game). A failed command rejects with the service's
//   `{code, message}` (client/core/src/ipc.rs, `ErrorBody`); every code is
//   translated here.
// - Events: `connection` (the service can or can't be reached), `status`
//   (client/core/src/engine/types.rs, `Status`, plus `owner`), `sample`
//   (one point for the live graph) and `action_error` (the tray's "Turn
//   Optimize off" failed).
//
// Every string shown comes from i18n/en.json or i18n/ar.json through
// t("key"), always with a literal key, so tools/check-i18n.py can check
// that both files have it. Numbers, times and units follow the language,
// and Arabic turns the page right-to-left.

"use strict";

(function () {
  const tauri = window.__TAURI__;
  const invoke = (command, args) => tauri.core.invoke(command, args);

  // The page's own failures go to the app's log, which the diagnostic
  // report includes: the WebView's console is kept nowhere.
  function reportPageError(error) {
    const message = String((error && error.message) || error);
    const stack = error && error.stack ? String(error.stack) : null;
    invoke("log_page_error", { message, stack }).catch(() => {});
  }
  window.addEventListener("error", (event) => reportPageError(event.error || event.message));
  window.addEventListener("unhandledrejection", (event) => reportPageError(event.reason));
  const listen = (name, handler) => tauri.event.listen(name, (event) => handler(event.payload));

  /** A session is on: the engine refuses to measure, and only its owner may change it. */
  const ACTIVE = new Set(["connecting", "optimized", "failing_over"]);
  /** After this long optimized with no game packets, the player is told to rejoin. */
  const NO_TRAFFIC_AFTER_MS = 20 * 1000;
  /** The ranges the service accepts (client/core/src/config.rs, `validate`). */
  const LIMITS = { ms: [0, 1000], pct: [0, 100] };
  /** Errors whose message from the service adds something the translation can't. */
  const DETAIL_CODES = new Set(["control", "connect", "platform", "invalid_settings", "cannot_save", "bad_request", "failed"]);
  // Unicode isolates: a game name, a number or a path dropped into a
  // sentence keeps its own direction instead of reordering the sentence.
  const FSI = "⁨";
  const PDI = "⁩";

  const state = {
    lang: "en",
    locale: "en",
    dicts: {},
    languagePref: "system",
    appVersion: "",
    connection: { state: "connecting" },
    connectionAt: Date.now(),
    games: [],
    status: null,
    settings: null,
    acks: new Set(),
    game: null,
    region: null,
    userChose: false,
    measuring: false,
    /** "on" or "off" while an enable or disable request is on its way. */
    switching: null,
    reporting: false,
    savingThresholds: false,
    thresholdsDirty: false,
    dismissing: new Set(),
    dialogGame: null,
    view: "main",
    errors: { measure: null, optimize: null, notices: null, verbose: null, language: null },
    thresholdsStatus: null,
    reportStatus: null,
  };

  let graph = null;
  const $ = (id) => document.getElementById(id);

  // ---- Translations and formats --------------------------------------

  async function loadDict(lang) {
    if (state.dicts[lang]) {
      return;
    }
    const response = await fetch(`i18n/${lang}.json`);
    state.dicts[lang] = await response.json();
  }

  function lookup(key) {
    const own = (state.dicts[state.lang] || {})[key];
    if (typeof own === "string" && own !== "") {
      return own;
    }
    const english = (state.dicts.en || {})[key];
    if (typeof english === "string" && english !== "") {
      return english;
    }
    console.warn(`missing translation: ${key}`);
    return key;
  }

  /** The string for `key`, with `{name}` replaced from `params`. */
  function t(key, params, options) {
    let text = lookup(key);
    if (params) {
      const isolate = !options || options.isolate !== false;
      text = text.replace(/\{(\w+)\}/g, (match, name) => {
        if (!(name in params)) {
          return match;
        }
        const value = String(params[name]);
        return isolate ? FSI + value + PDI : value;
      });
    }
    return text;
  }

  const formats = new Map();

  function numberFormat(options) {
    const id = state.locale + JSON.stringify(options);
    if (!formats.has(id)) {
      let format;
      try {
        format = new Intl.NumberFormat(state.locale, options);
      } catch {
        format = new Intl.NumberFormat(state.lang, options);
      }
      formats.set(id, format);
    }
    return formats.get(id);
  }

  const num = (value, digits = 0) =>
    numberFormat({ maximumFractionDigits: digits, minimumFractionDigits: 0 }).format(value);
  const known = (value) => typeof value === "number" && Number.isFinite(value);
  const ms = (value) => (known(value) ? t("unit.ms", { value: num(Math.round(value)) }) : t("value.unknown"));
  const pct = (value, digits = 1) => (known(value) ? t("unit.percent", { value: num(value, digits) }) : t("value.unknown"));

  function time(unixMs) {
    const id = `time:${state.locale}`;
    if (!formats.has(id)) {
      let format;
      try {
        format = new Intl.DateTimeFormat(state.locale, { hour: "numeric", minute: "2-digit" });
      } catch {
        format = new Intl.DateTimeFormat(state.lang, { hour: "numeric", minute: "2-digit" });
      }
      formats.set(id, format);
    }
    return formats.get(id).format(new Date(unixMs));
  }

  /** The Windows display language when it is the language in use (e.g. ar-SA
   * for its digits), otherwise the bare language. */
  function localeFor(lang, system) {
    if (system && system.toLowerCase().startsWith(lang)) {
      try {
        if (Intl.NumberFormat.supportedLocalesOf([system]).length > 0) {
          return system;
        }
      } catch {
        // Not a valid language tag.
      }
    }
    return lang;
  }

  /** A number for an input field: no grouping, so what is shown reads back
   * as the same number (with grouping, 1000 would show as "1,000", or
   * "1.000" in an Arabic (Morocco) Windows, whose decimal mark is ","). */
  const plain = (value) =>
    numberFormat({ useGrouping: false, maximumFractionDigits: 1, minimumFractionDigits: 0 }).format(value);

  /** The current locale's decimal and grouping marks. */
  function separators() {
    const parts = numberFormat({ useGrouping: true, minimumFractionDigits: 1 }).formatToParts(12345.5);
    const part = (type) => (parts.find((p) => p.type === type) || {}).value || "";
    return { decimal: part("decimal") || ".", group: part("group") };
  }

  /** A number the player typed, in Latin, Arabic-Indic or Persian digits,
   * with the language's own decimal and grouping marks (a Latin comma
   * counts as the decimal mark where it isn't the grouping one); NaN if it
   * isn't a plain number. */
  function parseNumber(text) {
    const { decimal, group } = separators();
    let normalized = String(text)
      .trim()
      .replace(/[\u0660-\u0669]/g, (d) => String(d.charCodeAt(0) - 0x0660))
      .replace(/[\u06f0-\u06f9]/g, (d) => String(d.charCodeAt(0) - 0x06f0))
      // Spaces, percent signs and direction marks.
      .replace(/[\s\u00a0\u202f%\u066a\u061c\u200e\u200f]/g, "")
      .replaceAll("\u066c", "");
    if (group && group !== decimal) {
      normalized = normalized.replaceAll(group, "");
    }
    normalized = normalized.replaceAll(decimal, ".").replaceAll("\u066b", ".");
    if (group !== ",") {
      // Typed for the decimal mark on many keyboards.
      normalized = normalized.replaceAll(",", ".");
    }
    if (normalized === "" || !/^\d*\.?\d+$|^\d+\.$/.test(normalized)) {
      return NaN;
    }
    return Number(normalized);
  }

  // ---- Translated texts for codes from the service --------------------

  function connectionReason(reason) {
    switch (reason) {
      case "not_running":
        return t("connection.not_running");
      case "access_denied":
        return t("connection.access_denied");
      case "busy":
        return t("connection.busy");
      case "lost":
        return t("connection.lost");
      case "failed":
        return t("connection.failed");
      case "version_mismatch":
        return t("connection.version_mismatch");
      case "too_many_connections":
        return t("connection.too_many_connections");
      default:
        return t("connection.refused");
    }
  }

  function errorMessage(code) {
    switch (code) {
      case "owned_by_another_user":
        return t("error.owned_by_another_user");
      case "hello_required":
        return t("error.hello_required");
      case "version_mismatch":
        return t("error.version_mismatch");
      case "rate_limited":
        return t("error.rate_limited");
      case "too_many_requests":
        return t("error.too_many_requests");
      case "too_many_connections":
        return t("error.too_many_connections");
      case "invalid_settings":
        return t("error.invalid_settings");
      case "cannot_save":
        return t("error.cannot_save");
      case "bad_request":
        return t("error.bad_request");
      case "refused":
        return t("error.refused");
      case "not_measured":
        return t("error.not_measured");
      case "direct_unknown":
        return t("error.direct_unknown");
      case "not_usual_region":
        return t("error.not_usual_region");
      case "warning_not_acknowledged":
        return t("error.warning_not_acknowledged");
      case "no_path":
        return t("error.no_path");
      case "not_better":
        return t("error.not_better");
      case "busy":
        return t("error.busy");
      case "control":
        return t("error.control");
      case "connect":
        return t("error.connect");
      case "platform":
        return t("error.platform");
      case "stopped":
        return t("error.stopped");
      case "service_unavailable":
        return t("error.service_unavailable");
      case "disconnected":
        return t("error.disconnected");
      case "timeout":
        return t("error.timeout");
      case "failed":
        return t("error.failed");
      case "report_failed":
        return t("error.report_failed");
      default:
        return null;
    }
  }

  /** A failed command, in the player's language; the service's own words
   * follow where they add something (they are English). */
  function errorText(error) {
    const code = error && typeof error === "object" ? error.code : null;
    const message = error && typeof error === "object" ? error.message : String(error);
    const translated = errorMessage(code);
    let text = translated || t("error.unknown", { code: code || "?" });
    if (message && (!translated || DETAIL_CODES.has(code))) {
      text += `\n${t("error.details", { message })}`;
    }
    return text;
  }

  function refusalText(refusal) {
    const r = refusal || {};
    switch (r.reason) {
      case "not_routable":
        return r.status === "unsupported"
          ? t("refusal.not_routable_unsupported", { game: r.game })
          : t("refusal.not_routable_untested", { game: r.game });
      case "unknown_region":
        return t("refusal.unknown_region", { game: r.game, region: r.region });
      case "unknown_game":
        return t("refusal.unknown_game", { game: r.game });
      case "shares_ranges_with_refused_game":
        return t("refusal.shares_ranges_with_refused_game", { game: r.game });
      default:
        return t("error.refused");
    }
  }

  function causeText(cause) {
    switch (cause) {
      case "relays_unreachable":
        return t("notice.cause.relays_unreachable");
      case "exit_not_forwarding":
        return t("notice.cause.exit_not_forwarding");
      case "session_expired":
        return t("notice.cause.session_expired");
      case "session_ended":
        return t("notice.cause.session_ended");
      case "renewal_refused":
        return t("notice.cause.renewal_refused");
      case "engine_stalled":
        return t("notice.cause.engine_stalled");
      case "adapter_lost":
        return t("notice.cause.adapter_lost");
      case "suspended":
        return t("notice.cause.suspended");
      default:
        return "";
    }
  }

  /** What a notice says, and how loudly. */
  function noticeView(notice) {
    switch (notice.code) {
      case "fell_back": {
        const cause = causeText(notice.cause);
        return { level: "warn", text: cause ? `${t("notice.fell_back")} ${cause}` : t("notice.fell_back") };
      }
      case "address_changed":
        return { level: "warn", text: t("notice.address_changed") };
      case "failed_over":
        return {
          level: "info",
          text: notice.entry ? t("notice.failed_over", { entry: notice.entry }) : t("notice.failed_over_direct"),
        };
      case "not_better":
        return {
          level: "info",
          text: t("notice.not_better", { direct: ms(notice.direct_ms), optimized: ms(notice.optimized_ms) }),
        };
      case "direct_unknown":
        return { level: "info", text: t("notice.direct_unknown") };
      case "refused":
        return { level: "warn", text: t("notice.refused", { reason: refusalText(notice.refusal) }) };
      case "game_traffic_blocked":
        return { level: "warn", text: t("notice.game_traffic_blocked", { packets: num(notice.packets || 0) }) };
      case "error":
        return { level: "error", text: t("notice.error", { message: notice.message || "" }) };
      default:
        return { level: "error", text: t("notice.error", { message: String(notice.code) }) };
    }
  }

  function statusBadge(status) {
    switch (status) {
      case "supported":
        return t("game.status.supported");
      case "testing":
        return t("game.status.testing");
      case "unsupported":
        return t("game.status.unsupported");
      default:
        return t("game.status.untested");
    }
  }

  function gameReason(game) {
    switch (game.status) {
      case "untested":
        return t("game.reason.untested");
      case "unsupported":
        return t("game.reason.unsupported");
      default:
        return t("game.reason.shares_ranges");
    }
  }

  /** One case per engine `Mode` (tools/check-i18n.py checks there is one). */
  function modeText(mode) {
    switch (mode) {
      case "idle":
        return t("optimize.mode.idle");
      case "measuring":
        return t("optimize.mode.measuring");
      case "ready":
        return t("optimize.mode.ready");
      case "connecting":
        return t("optimize.mode.connecting");
      case "optimized":
        return t("optimize.mode.optimized");
      case "failing_over":
        return t("optimize.mode.failing_over");
      default:
        // A newer service's mode: say what is certain.
        return t("optimize.mode.idle");
    }
  }

  // ---- DOM helpers ------------------------------------------------------

  function el(tag, props, ...children) {
    const node = document.createElement(tag);
    for (const [name, value] of Object.entries(props || {})) {
      if (value === null || value === undefined || value === false) {
        continue;
      }
      if (name === "class") {
        node.className = value;
      } else if (name === "text") {
        node.textContent = value;
      } else if (name.startsWith("on")) {
        node.addEventListener(name.slice(2), value);
      } else {
        node.setAttribute(name, value === true ? "" : String(value));
      }
    }
    node.append(...children.filter((child) => child !== null && child !== undefined));
    return node;
  }

  function show(node, visible) {
    node.hidden = !visible;
  }

  function setText(node, text) {
    if (node.textContent !== text) {
      node.textContent = text;
    }
  }

  function setError(node, error) {
    show(node, Boolean(error));
    setText(node, error ? errorText(error) : "");
  }

  /** Rebuilds a list only when what it shows changed, so focus and hover
   * survive the status updates that arrive every second. */
  function rebuild(node, signature, build) {
    const key = JSON.stringify(signature);
    if (node.dataset.signature === key) {
      return;
    }
    node.dataset.signature = key;
    const focused = document.activeElement && node.contains(document.activeElement)
      ? document.activeElement.dataset.id
      : null;
    node.replaceChildren(...build());
    if (focused) {
      const again = node.querySelector(`[data-id="${CSS.escape(focused)}"]`);
      if (again) {
        again.focus();
      }
    }
  }

  /** A value with its unit in a smaller type: `unit.ms` is "{value} ms". */
  function setMeasure(node, value) {
    if (!known(value)) {
      node.replaceChildren(document.createTextNode(t("value.unknown")));
      return;
    }
    const [before, after] = lookup("unit.ms").split("{value}");
    const parts = [document.createTextNode(num(Math.round(value)))];
    if (before) {
      parts.unshift(el("span", { class: "unit", text: before }));
    }
    if (after) {
      parts.push(el("span", { class: "unit", text: after }));
    }
    node.replaceChildren(...parts);
  }

  function translateStatic() {
    for (const node of document.querySelectorAll("[data-i18n]")) {
      setText(node, t(node.dataset.i18n));
    }
    for (const node of document.querySelectorAll("[data-i18n-aria-label]")) {
      node.setAttribute("aria-label", t(node.dataset.i18nAriaLabel));
    }
  }

  // ---- What the status means for the page -------------------------------

  function currentGame() {
    return state.games.find((game) => game.id === state.game) || null;
  }

  function derive() {
    const s = state.status;
    const mode = s ? s.mode : "idle";
    const active = ACTIVE.has(mode);
    const connected = state.connection.state === "connected";
    const owner = s ? s.owner : null;
    const gameMeasurement = s && s.measurement && s.measurement.game === state.game ? s.measurement : null;
    const measurement = gameMeasurement && gameMeasurement.region === state.region ? gameMeasurement : null;
    const regionView = gameMeasurement
      ? (gameMeasurement.regions || []).find((r) => r.region === state.region) || null
      : null;
    return {
      s,
      mode,
      active,
      connected,
      owner,
      othersSession: active && owner === "another_user",
      gameMeasurement,
      measurement,
      regionView,
      measuring: state.measuring || mode === "measuring",
    };
  }

  /** Follows the service's game and region while a session runs or a
   * measurement is under way, and until the player picks their own. */
  function syncSelection() {
    const s = state.status;
    if (s && s.game && (ACTIVE.has(s.mode) || s.mode === "measuring" || !state.userChose)) {
      if (state.game !== s.game) {
        state.game = s.game;
        state.region = null;
      }
      if (s.region) {
        state.region = s.region;
      }
    }
    // A game that is never routed can't be picked (rule 7); with none
    // left, nothing is selected.
    const game = currentGame();
    if (!game || !game.routable) {
      const first = state.games.find((candidate) => candidate.routable);
      state.game = first ? first.id : null;
      state.region = null;
    }
    const picked = currentGame();
    if (picked && !picked.regions.some((r) => r.id === state.region)) {
      state.region = s && s.game === picked.id && s.region ? s.region : picked.regions.length > 0 ? picked.regions[0].id : null;
    }
  }

  // ---- Rendering ----------------------------------------------------------

  function render() {
    const d = derive();
    renderConnection();
    renderNotices(d);
    renderGames(d);
    renderRegions(d);
    renderCompare(d);
    renderOptimize(d);
    renderSettings(d);
    renderGraphEmpty();
  }

  function renderConnection() {
    const c = state.connection;
    const pill = $("connection-pill");
    pill.classList.toggle("is-connected", c.state === "connected");
    pill.classList.toggle("is-unavailable", c.state === "unavailable");
    setText(
      pill,
      c.state === "connected"
        ? t("connection.pill_connected")
        : c.state === "unavailable"
          ? t("connection.pill_unavailable")
          : t("connection.pill_connecting"),
    );
    const unavailable = c.state === "unavailable";
    show($("banner"), unavailable);
    if (!unavailable) {
      return;
    }
    setText($("banner-reason"), connectionReason(c.reason));
    const left = known(c.retry_in_ms) ? Math.ceil((state.connectionAt + c.retry_in_ms - Date.now()) / 1000) : 0;
    setText($("banner-retry"), left > 0 ? t("connection.retry_in", { seconds: num(left) }) : t("connection.connecting"));
    const detail = $("banner-detail");
    show(detail, Boolean(c.detail));
    setText(detail, c.detail ? t("connection.details", { detail: c.detail }) : "");
  }

  function renderNotices(d) {
    const box = $("notices");
    const notices = d.s && Array.isArray(d.s.notices) ? d.s.notices.slice().reverse() : [];
    // The service refuses dismissals from anyone but the session's owner.
    const canDismiss = d.connected && d.owner !== "another_user";
    const error = state.errors.notices;
    rebuild(box, [state.lang, state.locale, notices, canDismiss, d.owner, [...state.dismissing], error], () => {
      const items = notices.map((notice) => {
        const view = noticeView(notice);
        const level = view.level === "warn" ? " is-warn" : view.level === "error" ? " is-error" : "";
        return el(
          "article",
          { class: `notice${level}`, "data-id": notice.id },
          el(
            "div",
            { class: "notice-text" },
            el("p", { text: view.text }),
            known(notice.at_unix) ? el("span", { class: "notice-time", text: time(notice.at_unix * 1000) }) : null,
          ),
          el("button", {
            class: "button",
            type: "button",
            "data-id": `dismiss-${notice.id}`,
            disabled: !canDismiss || state.dismissing.has(notice.id),
            text: t("notice.dismiss"),
            onclick: () => dismiss(notice.id),
          }),
        );
      });
      if (notices.length > 0 && d.owner === "another_user") {
        items.push(el("p", { class: "note", text: t("notice.another_user") }));
      }
      if (error) {
        items.push(el("p", { class: "error", role: "alert", text: errorText(error) }));
      }
      return items;
    });
  }

  function renderGames(d) {
    const list = $("game-list");
    const locked = d.active || d.measuring || !d.connected;
    show($("game-empty"), state.games.length === 0);
    show($("game-none-routable"), state.games.length > 0 && !state.games.some((game) => game.routable));
    rebuild(list, [state.lang, state.games, state.game, locked], () =>
      state.games.map((game) => {
        const selected = game.id === state.game;
        const badges = [el("span", { class: `badge badge-${game.status}`, text: statusBadge(game.status) })];
        if (game.test_definition) {
          badges.push(el("span", { class: "badge", text: t("game.test_definition") }));
        }
        return el(
          "button",
          {
            class: `choice${game.routable ? "" : " is-blocked"}`,
            type: "button",
            role: "radio",
            "aria-checked": selected ? "true" : "false",
            "data-id": game.id,
            disabled: !game.routable || (locked && !selected),
            onclick: () => selectGame(game.id),
          },
          el(
            "span",
            { class: "choice-head" },
            el("span", { class: "choice-name", dir: "auto", text: game.name }),
            el("span", { class: "badges" }, ...badges),
          ),
          game.routable ? null : el("span", { class: "choice-reason", text: gameReason(game) }),
        );
      }),
    );
  }

  function renderRegions(d) {
    const game = currentGame();
    const list = $("region-list");
    const locked = d.active || d.measuring || !d.connected || !game || !game.routable;
    const regions = game ? game.regions : [];
    const measured = d.gameMeasurement ? d.gameMeasurement.regions || [] : [];
    show($("region-section"), Boolean(game));
    show($("region-note"), measured.some((r) => !r.usual));
    rebuild(list, [state.lang, state.locale, regions, measured, state.region, locked], () =>
      regions.map((region) => {
        const seen = measured.find((r) => r.region === region.id);
        const far = Boolean(seen && !seen.usual);
        const selected = region.id === state.region;
        const ping = seen && seen.direct ? ms(seen.direct.rtt_ms) : null;
        const label = far
          ? t("region.not_usual", { name: region.name })
          : ping
            ? t("region.with_ping", { name: region.name, ping })
            : region.name;
        return el(
          "button",
          {
            class: `chip${far ? " is-far" : ""}`,
            type: "button",
            role: "radio",
            "aria-checked": selected ? "true" : "false",
            "aria-label": label,
            title: far ? label : null,
            "data-id": region.id,
            disabled: (locked && !selected) || (far && !selected),
            onclick: () => selectRegion(region.id),
          },
          el("span", { class: "chip-name", dir: "auto", text: region.name }),
          seen ? el("span", { class: "chip-sub", text: far ? `${ping || t("value.unknown")} · ${t("region.far")}` : ping || t("value.unknown") }) : null,
        );
      }),
    );

    const measure = $("measure");
    measure.disabled = !d.connected || d.active || d.measuring || !game || !game.routable || !state.region;
    setText(measure, d.measuring ? t("measure.measuring") : t("measure.button"));
    show($("lock-note"), d.active && !d.othersSession);
    setError($("measure-error"), state.errors.measure);
  }

  function renderCompare(d) {
    const m = d.measurement;
    const path = d.active && d.s ? d.s.path : null;
    const direct = path && known(path.direct_ms) ? path.direct_ms : m && m.direct ? m.direct.rtt_ms : null;
    // A region the player doesn't normally use is never optimized (rule 3),
    // so no estimate through PingPath is offered for it, even though the
    // engine works one out.
    const far = !path && Boolean(d.regionView && !d.regionView.usual);
    const best = m && m.best && !far ? m.best : null;
    const measuredThrough = path && known(path.optimized_ms);
    const optimized = measuredThrough ? path.optimized_ms : best ? best.estimate_ms : null;

    setMeasure($("direct-value"), direct);
    setText($("direct-loss"), m && m.direct && m.direct.loss_pct > 0 ? t("compare.loss", { value: pct(m.direct.loss_pct) }) : "");
    setText($("optimized-label"), measuredThrough ? t("compare.measured") : t("compare.estimated"));
    setMeasure($("optimized-value"), optimized);
    const route = path || best;
    setText(
      $("optimized-sub"),
      route ? (route.entry ? t("session.path_entry", { entry: route.entry, exit: route.exit }) : t("session.path_direct_exit", { exit: route.exit })) : "",
    );

    const verdict = $("compare-verdict");
    let text;
    let good = false;
    if (!m && !path) {
      text = d.measuring ? t("measure.measuring") : currentGame() ? t("measure.hint") : "";
    } else if (far) {
      text = t("compare.not_usual");
    } else if (!known(direct)) {
      text = t("compare.direct_unknown");
    } else if (!known(optimized)) {
      text = t("compare.no_path");
    } else if (path || (m && m.worthwhile)) {
      const gain = direct - optimized;
      if (gain > 0) {
        const values = { ms: ms(gain), pct: pct((gain / direct) * 100, 0) };
        text = measuredThrough ? t("compare.better_measured", values) : t("compare.better", values);
        good = true;
      } else {
        text = t("compare.not_better");
      }
    } else {
      text = t("compare.not_better");
    }
    setText(verdict, text);
    verdict.classList.toggle("is-good", good);
    setText($("compare-time"), m && known(m.measured_at_unix) ? t("compare.measured_at", { time: time(m.measured_at_unix * 1000) }) : "");
  }

  /** Why Optimize can't be turned on now, if it can't. */
  function blockedReason(d) {
    if (!currentGame()) {
      return null;
    }
    if (!d.measurement) {
      return d.measuring ? null : t("optimize.measure_first");
    }
    if (!d.measurement.direct) {
      return t("error.direct_unknown");
    }
    if (d.regionView && !d.regionView.usual) {
      return t("error.not_usual_region");
    }
    if (!d.measurement.best) {
      return t("error.no_path");
    }
    if (!d.measurement.worthwhile) {
      return t("optimize.not_better");
    }
    return null;
  }

  function renderOptimize(d) {
    const toggle = $("optimize");
    const on = d.active;
    let disabled;
    let reason = null;
    if (!d.connected) {
      disabled = true;
    } else if (on) {
      disabled = d.othersSession || state.switching !== null;
    } else if (d.measuring || state.switching !== null) {
      disabled = true;
    } else {
      reason = blockedReason(d);
      disabled = reason !== null || d.mode !== "ready";
    }
    toggle.setAttribute("aria-checked", on ? "true" : "false");
    toggle.disabled = disabled;
    toggle.classList.toggle("is-busy", state.switching !== null || d.mode === "connecting" || d.mode === "failing_over");

    // While a request is on its way, say what it is doing; the service's
    // status takes over once it answers.
    let stateText = modeText(d.mode);
    if (state.connection.state === "unavailable") {
      // Without the service there is no status: a session may still be
      // running (the service refused this window, say), so "Off" would
      // be a guess.
      stateText = t("optimize.unknown");
    } else if (state.switching === "off") {
      stateText = t("optimize.turning_off");
    } else if (state.switching === "on" && !on) {
      stateText = t("optimize.mode.connecting");
    }
    const stateNode = $("optimize-state");
    setText(stateNode, stateText);
    stateNode.classList.toggle("is-on", d.mode === "optimized");

    const reasonNode = $("optimize-reason");
    show(reasonNode, Boolean(reason) && d.connected);
    setText(reasonNode, reason || "");
    show($("optimize-owner"), d.othersSession);
    setError($("optimize-error"), state.errors.optimize);

    const path = d.active && d.s ? d.s.path : null;
    show($("session"), Boolean(path));
    if (path) {
      setText(
        $("session-path"),
        path.entry ? t("session.path_entry", { entry: path.entry, exit: path.exit }) : t("session.path_direct_exit", { exit: path.exit }),
      );
      setText($("session-since"), known(path.since_unix) ? time(path.since_unix * 1000) : t("value.unknown"));
      setText($("session-packets"), num(d.s.carried || 0));
    }
    renderSessionHint(d);
  }

  function renderSessionHint(d) {
    const path = d.active && d.s ? d.s.path : null;
    // Only the session's owner can rejoin their match.
    const quiet =
      !d.othersSession &&
      d.mode === "optimized" &&
      path &&
      (d.s.carried || 0) === 0 &&
      known(path.since_unix) &&
      Date.now() - path.since_unix * 1000 >= NO_TRAFFIC_AFTER_MS;
    show($("session-hint"), Boolean(quiet));
  }

  function renderSettings(d) {
    for (const button of $("language-choice").querySelectorAll("[data-language]")) {
      button.setAttribute("aria-checked", button.dataset.language === state.languagePref ? "true" : "false");
    }
    setError($("language-error"), state.errors.language);

    const settings = state.settings;
    const usable = d.connected && settings !== null;
    show($("settings-unavailable"), !usable);
    const verbose = $("verbose");
    verbose.setAttribute("aria-checked", settings && settings.verbose_logging ? "true" : "false");
    verbose.disabled = !usable;
    setError($("verbose-error"), state.errors.verbose);

    const minMs = $("min-ms");
    const minPct = $("min-pct");
    for (const input of [minMs, minPct]) {
      input.disabled = !usable || state.savingThresholds;
    }
    $("thresholds-save").disabled = !usable || state.savingThresholds;
    if (settings && !state.thresholdsDirty) {
      minMs.value = plain(settings.min_improvement_ms);
      minPct.value = plain(settings.min_improvement_pct);
    }
    const thresholds = $("thresholds-status");
    const ts = state.thresholdsStatus;
    setText(thresholds, ts ? ts.text : "");
    thresholds.className = `small${ts ? (ts.ok ? " status-ok" : " status-error") : ""}`;

    setText($("control-url"), settings ? settings.control_url || t("settings.control_url_unset") : t("value.unknown"));

    const report = $("report");
    report.disabled = state.reporting;
    const rs = state.reportStatus;
    const reportNode = $("report-status");
    setText(reportNode, rs ? rs.text : "");
    reportNode.className = `small${rs ? (rs.ok ? " status-ok" : " status-error") : ""}`;

    setText($("version-app"), state.appVersion || t("value.unknown"));
    setText($("version-service"), state.connection.service_version || t("value.unknown"));
  }

  function renderGraphEmpty() {
    show($("graph-empty"), !graph || !graph.hasData());
  }

  function setGraphFormat() {
    graph.setFormat({
      number: (value) => num(value),
      percent: (value) => t("unit.percent", { value: num(value) }, { isolate: false }),
      ago: (seconds) => t("graph.seconds_ago", { value: num(seconds) }, { isolate: false }),
      now: t("graph.now"),
      msUnit: t("graph.axis_ms"),
      rtl: state.lang === "ar",
    });
  }

  // ---- Language -------------------------------------------------------

  async function applyLanguage(info) {
    const lang = info && info.language === "ar" ? "ar" : "en";
    await loadDict("en");
    await loadDict(lang);
    state.lang = lang;
    state.languagePref = (info && info.preference) || "system";
    state.locale = localeFor(lang, info && info.system);
    formats.clear();
    const root = document.documentElement;
    root.lang = lang;
    root.dir = lang === "ar" ? "rtl" : "ltr";
    translateStatic();
    if (state.dialogGame) {
      fillWarning(state.dialogGame);
    }
    if (graph) {
      setGraphFormat();
    }
    state.thresholdsDirty = false;
    render();
  }

  async function chooseLanguage(preference) {
    state.errors.language = null;
    try {
      await applyLanguage(await invoke("set_language", { preference }));
    } catch (error) {
      // The choice applies even if it couldn't be saved.
      state.errors.language = error;
      try {
        await applyLanguage(await invoke("get_language"));
      } catch {
        render();
      }
    }
  }

  // ---- Talking to the service --------------------------------------------

  async function refreshGames() {
    try {
      const games = await invoke("games");
      state.games = Array.isArray(games) ? games : [];
    } catch (error) {
      console.warn("games", error);
    }
  }

  async function refreshStatus() {
    try {
      state.status = await invoke("status");
    } catch (error) {
      console.warn("status", error);
    }
  }

  async function refreshSettings() {
    try {
      state.settings = await invoke("settings");
    } catch (error) {
      console.warn("settings", error);
    }
  }

  async function refreshAll() {
    await Promise.all([refreshGames(), refreshStatus(), refreshSettings()]);
    syncSelection();
    render();
  }

  function onConnection(connection) {
    if (!connection || typeof connection !== "object") {
      return;
    }
    const was = state.connection.state;
    state.connection = connection;
    state.connectionAt = Date.now();
    if (connection.state === "connected" && was !== "connected") {
      refreshAll();
    } else if (connection.state !== "connected") {
      // Whatever session there was is out of this window's reach.
      state.status = null;
    }
    render();
  }

  function onStatus(status) {
    if (!status || typeof status !== "object") {
      return;
    }
    state.status = status;
    syncSelection();
    render();
  }

  function onSample(sample) {
    if (graph) {
      graph.push(sample);
      renderGraphEmpty();
    }
  }

  function selectGame(id) {
    if (id === state.game) {
      return;
    }
    state.userChose = true;
    state.game = id;
    state.region = null;
    state.errors.measure = null;
    state.errors.optimize = null;
    syncSelection();
    render();
  }

  function selectRegion(id) {
    if (id === state.region) {
      return;
    }
    state.userChose = true;
    state.region = id;
    state.errors.measure = null;
    state.errors.optimize = null;
    render();
  }

  async function measure() {
    if (!state.game || !state.region || state.measuring) {
      return;
    }
    state.userChose = true;
    state.measuring = true;
    state.errors.measure = null;
    state.errors.optimize = null;
    render();
    try {
      await invoke("measure", { game: state.game, region: state.region });
    } catch (error) {
      state.errors.measure = error;
    }
    state.measuring = false;
    await refreshStatus();
    syncSelection();
    render();
  }

  /** Runs enable or disable (`direction` "on" or "off"); the answer is
   * the new status. */
  async function switchOptimize(direction, run) {
    state.switching = direction;
    state.errors.optimize = null;
    render();
    try {
      const status = await run();
      if (status && typeof status === "object") {
        state.status = status;
      }
    } catch (error) {
      state.errors.optimize = error;
      await refreshStatus();
    }
    state.switching = null;
    syncSelection();
    render();
  }

  async function toggleOptimize() {
    const d = derive();
    if (d.active) {
      await switchOptimize("off", () => invoke("disable"));
      return;
    }
    const game = currentGame();
    if (!game) {
      return;
    }
    const needsWarning = Boolean((d.measurement && d.measurement.needs_warning) || game.needs_warning);
    if (needsWarning && !state.acks.has(game.id)) {
      if (!(await askWarning(game))) {
        return;
      }
      state.acks.add(game.id);
      try {
        await invoke("acknowledge_game", { game: game.id });
      } catch (error) {
        // Not remembered: asked again next time, but it applies now.
        console.warn("acknowledge_game", error);
      }
    }
    await switchOptimize("on", () => invoke("enable", { acknowledged_warning: needsWarning }));
  }

  async function dismiss(id) {
    state.dismissing.add(id);
    state.errors.notices = null;
    render();
    try {
      await invoke("dismiss", { id });
      if (state.status && Array.isArray(state.status.notices)) {
        state.status.notices = state.status.notices.filter((notice) => notice.id !== id);
      }
    } catch (error) {
      state.errors.notices = error;
    }
    state.dismissing.delete(id);
    render();
  }

  // ---- The warning for games still being tested ----------------------

  function fillWarning(game) {
    // The warning may be another game's: one still being tested whose
    // servers this game shares, so its traffic would be carried too.
    const m = state.status && state.status.measurement;
    const name = (m && m.game === game.id && m.warning_game) || game.name;
    setText($("warning-title"), t("warning.title", { game: name }));
    setText($("warning-body"), t("warning.body", { game: name }));
  }

  /** Asks once per game; true if the player accepted. */
  function askWarning(game) {
    const dialog = $("warning-dialog");
    state.dialogGame = game;
    fillWarning(game);
    dialog.returnValue = "";
    return new Promise((resolve) => {
      dialog.addEventListener(
        "close",
        () => {
          state.dialogGame = null;
          resolve(dialog.returnValue === "accept");
        },
        { once: true },
      );
      dialog.showModal();
      $("warning-accept").focus();
    });
  }

  // ---- Settings ------------------------------------------------------------

  async function toggleVerbose() {
    const settings = state.settings;
    if (!settings) {
      return;
    }
    state.errors.verbose = null;
    const verbose = $("verbose");
    verbose.disabled = true;
    try {
      state.settings = await invoke("set_settings", { change: { verbose_logging: !settings.verbose_logging } });
    } catch (error) {
      state.errors.verbose = error;
      await refreshSettings();
    }
    render();
  }

  async function saveThresholds(event) {
    event.preventDefault();
    const minMs = $("min-ms");
    const minPct = $("min-pct");
    const msValue = parseNumber(minMs.value);
    const pctValue = parseNumber(minPct.value);
    const msOk = msValue >= LIMITS.ms[0] && msValue <= LIMITS.ms[1];
    const pctOk = pctValue >= LIMITS.pct[0] && pctValue <= LIMITS.pct[1];
    minMs.setAttribute("aria-invalid", msOk ? "false" : "true");
    minPct.setAttribute("aria-invalid", pctOk ? "false" : "true");
    if (!msOk || !pctOk) {
      state.thresholdsStatus = {
        ok: false,
        text: t("settings.out_of_range", { min: num(0), max_ms: num(LIMITS.ms[1]), max_pct: num(LIMITS.pct[1]) }),
      };
      render();
      return;
    }
    state.savingThresholds = true;
    state.thresholdsStatus = null;
    render();
    try {
      state.settings = await invoke("set_settings", {
        change: { min_improvement_ms: msValue, min_improvement_pct: pctValue },
      });
      state.thresholdsDirty = false;
      state.thresholdsStatus = { ok: true, text: t("settings.saved") };
    } catch (error) {
      state.thresholdsStatus = { ok: false, text: errorText(error) };
      state.thresholdsDirty = false;
      await refreshSettings();
    }
    state.savingThresholds = false;
    render();
  }

  async function copyReport() {
    state.reporting = true;
    state.reportStatus = null;
    render();
    try {
      const saved = await invoke("copy_report");
      const text =
        saved.copied && saved.path
          ? t("settings.report_copied_saved", { path: saved.path })
          : saved.copied
            ? t("settings.report_copied")
            : t("settings.report_saved", { path: saved.path || "" });
      state.reportStatus = { ok: true, text };
    } catch (error) {
      state.reportStatus = { ok: false, text: errorText(error) };
    }
    state.reporting = false;
    render();
  }

  function showView(view) {
    state.view = view;
    show($("view-main"), view === "main");
    show($("view-settings"), view === "settings");
    show($("nav-settings"), view === "main");
    show($("nav-back"), view === "settings");
    if (view === "settings") {
      state.thresholdsStatus = null;
      state.reportStatus = null;
      state.thresholdsDirty = false;
      if (state.connection.state === "connected") {
        refreshSettings().then(render);
      }
    } else if (graph) {
      graph.draw();
    }
    render();
    window.scrollTo(0, 0);
  }

  // ---- Start ---------------------------------------------------------------

  function wire() {
    $("measure").addEventListener("click", measure);
    $("optimize").addEventListener("click", toggleOptimize);
    $("banner-retry-button").addEventListener("click", () => {
      invoke("reconnect").catch((error) => console.warn("reconnect", error));
    });
    $("nav-settings").addEventListener("click", () => showView("settings"));
    $("nav-back").addEventListener("click", () => showView("main"));
    for (const button of $("language-choice").querySelectorAll("[data-language]")) {
      button.addEventListener("click", () => chooseLanguage(button.dataset.language));
    }
    $("verbose").addEventListener("click", toggleVerbose);
    $("thresholds").addEventListener("submit", saveThresholds);
    for (const input of [$("min-ms"), $("min-pct")]) {
      input.addEventListener("input", () => {
        state.thresholdsDirty = true;
        input.removeAttribute("aria-invalid");
      });
    }
    $("report").addEventListener("click", copyReport);
    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && state.view === "settings" && !$("warning-dialog").open) {
        showView("main");
      }
    });
  }

  function tick() {
    renderConnection();
    renderSessionHint(derive());
    if (graph && state.view === "main") {
      graph.draw();
      renderGraphEmpty();
    }
  }

  async function start() {
    graph = new window.PingPathGraph.LiveGraph($("graph"));
    wire();
    let language = { preference: "system", language: "en", system: null };
    try {
      language = await invoke("get_language");
    } catch (error) {
      console.warn("get_language", error);
    }
    await applyLanguage(language);
    try {
      state.appVersion = (await invoke("app_info")).version;
    } catch (error) {
      console.warn("app_info", error);
    }
    try {
      state.acks = new Set(await invoke("acknowledged_games"));
    } catch (error) {
      console.warn("acknowledged_games", error);
    }
    await Promise.all([
      listen("connection", onConnection),
      listen("status", onStatus),
      listen("sample", onSample),
      listen("action_error", (error) => {
        state.errors.optimize = error;
        render();
      }),
    ]);
    // Asked after listening, so no change in between is missed.
    try {
      onConnection(await invoke("connection"));
    } catch (error) {
      console.warn("connection", error);
    }
    render();
    setInterval(tick, 1000);
  }

  start().catch((error) => {
    console.error("PingPath window failed to start", error);
    reportPageError(error);
  });
})();
