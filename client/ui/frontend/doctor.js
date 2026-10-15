// The network doctor's page (docs/decisions/011-home-network-doctor.md):
// runs the service's checks (the `run_doctor` command), shows each one as
// its `doctor` event arrives, and turns every finding the service sends
// (a code with numbers and names) into a sentence and a fix in the
// player's language. The SQM guidance for routers is at the bottom.
//
// app.js creates it with its own helpers, so the strings come from the
// same translation files, through t() with literal keys.

"use strict";

window.PingPathDoctor = function (ui) {
  const { $, el, t, num, ms, show, setText, errorText, invoke, time, rebuild, language } = ui;
  const CHECKS = ["connection", "background", "dns", "bufferbloat"];
  const routers = () => window.PingPathRouters(t);

  const state = {
    running: false,
    /** This window's own run_doctor call is waiting for its reply. */
    own: false,
    /** The latest result of each check, by check. */
    results: {},
    /** Checks that started and haven't finished. */
    started: new Set(),
    finishedAt: null,
    error: null,
    maker: "other",
    linkError: null,
  };

  // ---- Words ---------------------------------------------------------------

  function title(check) {
    switch (check) {
      case "connection":
        return t("doctor.check.connection");
      case "background":
        return t("doctor.check.background");
      case "dns":
        return t("doctor.check.dns");
      default:
        return t("doctor.check.bufferbloat");
    }
  }

  function statusText(status) {
    switch (status) {
      case "waiting":
        return t("doctor.status.waiting");
      case "running":
        return t("doctor.status.running");
      case "good":
        return t("doctor.status.good");
      case "warning":
        return t("doctor.status.warning");
      case "problem":
        return t("doctor.status.problem");
      case "unavailable":
        return t("doctor.status.unavailable");
      default:
        return t("doctor.status.failed");
    }
  }

  /** A rate given in kbit/s, in the unit that reads best. */
  function rate(kbps) {
    if (typeof kbps !== "number" || !Number.isFinite(kbps)) {
      return t("value.unknown");
    }
    if (kbps >= 1000) {
      return t("unit.mbps", { value: num(kbps / 1000, kbps < 10000 ? 1 : 0) });
    }
    return t("unit.kbps", { value: num(Math.round(kbps)) });
  }

  /** A finding's parameters, formatted for its sentence: `*_kbps` become
   * rates named without the suffix ({down}, {up}), milliseconds go
   * through ms(), other numbers through num(). */
  function params(raw) {
    const out = {};
    for (const [name, value] of Object.entries(raw || {})) {
      if (name.endsWith("_kbps")) {
        out[name.slice(0, -"_kbps".length)] = rate(value);
      } else if (name === "ms" || name.endsWith("_ms")) {
        out[name] = ms(value);
      } else if (typeof value === "number") {
        out[name] = num(value);
      } else {
        out[name] = value;
      }
    }
    return out;
  }

  function gradeFix(p, grade) {
    if (grade === "A+" || grade === "A") {
      return t("doctor.finding.bufferbloat_grade_good_fix", p);
    }
    if (grade === "B") {
      return t("doctor.finding.bufferbloat_grade_fair_fix", p);
    }
    return t("doctor.finding.bufferbloat_grade_poor_fix", p);
  }

  /** [what was found, what to do about it (or null)] for each code. */
  const FINDINGS = {
    ethernet: (p) => [t("doctor.finding.ethernet", p), null],
    wifi: (p) => [t("doctor.finding.wifi", p), t("doctor.finding.wifi_fix", p)],
    weak_signal: (p) => [t("doctor.finding.weak_signal", p), t("doctor.finding.weak_signal_fix", p)],
    crowded_band: (p) => [t("doctor.finding.crowded_band", p), t("doctor.finding.crowded_band_fix", p)],
    slow_link: (p) => [t("doctor.finding.slow_link", p), t("doctor.finding.slow_link_fix", p)],
    virtual_adapter: (p) => [t("doctor.finding.virtual_adapter", p), t("doctor.finding.virtual_adapter_fix", p)],
    dns_none: (p) => [t("doctor.finding.dns_none", p), t("doctor.finding.dns_none_fix", p)],
    dns_unanswered: (p) => [t("doctor.finding.dns_unanswered", p), t("doctor.finding.dns_unanswered_fix", p)],
    dns_slow_cached: (p) => [t("doctor.finding.dns_slow_cached", p), t("doctor.finding.dns_slow_cached_fix", p)],
    dns_slow_first: (p) => [t("doctor.finding.dns_slow_first", p), t("doctor.finding.dns_slow_first_fix", p)],
    dns_faster_public: (p) => [t("doctor.finding.dns_faster_public", p), t("doctor.finding.dns_faster_public_fix", p)],
    dns_good: (p) => [t("doctor.finding.dns_good", p), null],
    bufferbloat_grade: (p, raw) => [t("doctor.finding.bufferbloat_grade", p), gradeFix(p, raw.grade)],
    sqm_suggested: (p) => [t("doctor.finding.sqm_suggested", p), t("doctor.finding.sqm_suggested_fix", p)],
    no_test_server: (p) => [t("doctor.finding.no_test_server", p), t("doctor.finding.no_test_server_fix", p)],
    test_limit_reached: (p) => [t("doctor.finding.test_limit_reached", p), t("doctor.finding.test_limit_reached_fix", p)],
    background_share: (p) => [t("doctor.finding.background_share", p), t("doctor.finding.background_share_fix", p)],
    stop_optimizing: (p) => [t("doctor.finding.stop_optimizing", p), t("doctor.finding.stop_optimizing_fix", p)],
    background_quiet: (p) => [t("doctor.finding.background_quiet", p), null],
    background_launcher: (p) => [t("doctor.finding.background_launcher", p), t("doctor.finding.background_launcher_fix", p)],
    background_sync: (p) => [t("doctor.finding.background_sync", p), t("doctor.finding.background_sync_fix", p)],
    background_update: (p) => [t("doctor.finding.background_update", p), t("doctor.finding.background_update_fix", p)],
    background_browser: (p) => [t("doctor.finding.background_browser", p), t("doctor.finding.background_browser_fix", p)],
    background_streaming: (p) => [t("doctor.finding.background_streaming", p), t("doctor.finding.background_streaming_fix", p)],
    background_other: (p) => [t("doctor.finding.background_other", p), t("doctor.finding.background_other_fix", p)],
    not_supported: (p) => [t("doctor.finding.not_supported", p), null],
    check_failed: (p) => [t("doctor.finding.check_failed", p), t("doctor.finding.check_failed_fix", p)],
  };

  // ---- What each check measured ----------------------------------------------

  function connectionSummary(f) {
    const adapter = f.adapter || t("value.unknown");
    if (f.medium === "ethernet") {
      // An adapter that doesn't say its speed sends null: unknown, not 0.
      const speed = typeof f.link_mbps === "number" ? rate(f.link_mbps * 1000) : t("value.unknown");
      return [t("doctor.summary.ethernet", { adapter, speed })];
    }
    if (f.medium !== "wifi") {
      return [t("doctor.summary.other", { adapter })];
    }
    const wifi = f.wifi || {};
    const details = [];
    const dbm = typeof wifi.rssi_dbm === "number" ? wifi.rssi_dbm : null;
    if (dbm !== null) {
      details.push(t("doctor.wifi.signal", { value: num(dbm) }));
    } else if (typeof wifi.quality_pct === "number") {
      details.push(t("doctor.wifi.quality", { value: num(wifi.quality_pct) }));
    }
    if (wifi.band) {
      details.push(t("doctor.wifi.band", { value: num(Number(wifi.band), 1) }));
    }
    if (wifi.generation) {
      details.push(t("doctor.wifi.generation", { value: num(wifi.generation) }));
    }
    const lines = [t("doctor.summary.wifi", { adapter })];
    if (details.length > 0) {
      lines.push(details.join(t("doctor.separator")));
    }
    return lines;
  }

  function summary(check, result) {
    const f = result && result.figures;
    if (!f) {
      return [];
    }
    switch (check) {
      case "connection":
        return connectionSummary(f);
      case "background":
        return [
          t("doctor.summary.background", {
            down: rate(f.total_down_kbps),
            up: rate(f.total_up_kbps),
            secs: num(f.window_secs),
          }),
        ];
      case "dns": {
        const mine = (f.resolvers || [])[0];
        return mine
          ? [t("doctor.summary.dns", { address: mine.address, first: ms(mine.first_ms), cached: ms(mine.cached_ms) })]
          : [];
      }
      default:
        return [];
    }
  }

  /** The bufferbloat test's grade and its three phases. */
  function bufferbloatTable(f) {
    const row = (label, phase, loaded) =>
      el(
        "tr",
        {},
        el("th", { scope: "row", text: label }),
        el("td", { text: ms(phase.median_ms) }),
        el("td", { text: loaded && typeof phase.increase_ms === "number" ? t("doctor.bb.increase", { value: ms(phase.increase_ms) }) : "" }),
        el("td", { text: loaded ? rate(phase.mbps * 1000) : "" }),
      );
    return el(
      "div",
      { class: "doctor-bufferbloat" },
      el("div", { class: `grade grade-${String(f.grade).replace("+", "plus")}`, "aria-label": t("doctor.bb.grade", { grade: f.grade }) }, f.grade),
      el(
        "table",
        { class: "doctor-table" },
        el(
          "thead",
          {},
          el(
            "tr",
            {},
            el("td", {}),
            el("th", { scope: "col", text: t("doctor.bb.latency") }),
            el("th", { scope: "col", text: t("doctor.bb.added") }),
            el("th", { scope: "col", text: t("doctor.bb.speed") }),
          ),
        ),
        el(
          "tbody",
          {},
          row(t("doctor.bb.idle"), f.idle, false),
          row(t("doctor.bb.download"), f.download, true),
          row(t("doctor.bb.upload"), f.upload, true),
        ),
      ),
      el("p", { class: "muted", text: t("doctor.bb.server", { server: f.server }) }),
    );
  }

  // ---- Rendering -------------------------------------------------------------

  function phaseOf(check) {
    const result = state.results[check];
    if (result && !state.started.has(check)) {
      return result.status;
    }
    if (state.started.has(check)) {
      return "running";
    }
    return state.running ? "waiting" : null;
  }

  function checkCard(check) {
    const result = state.results[check];
    const phase = phaseOf(check);
    const fresh = result && phase === result.status;
    const findings = fresh ? result.findings : [];
    return el(
      "section",
      { class: `card doctor-check doctor-${phase || "idle"}` },
      el(
        "div",
        { class: "doctor-check-head" },
        el("h3", { text: title(check) }),
        phase ? el("span", { class: `doctor-badge doctor-badge-${phase}`, text: statusText(phase) }) : null,
      ),
      ...(fresh ? summary(check, result).map((line) => el("p", { class: "doctor-summary", dir: "auto", text: line })) : []),
      fresh && check === "bufferbloat" && result.figures ? bufferbloatTable(result.figures) : null,
      findings.length > 0
        ? el(
            "ul",
            { class: "doctor-findings" },
            ...findings.map((finding) => {
              const words = FINDINGS[finding.code];
              const [found, fix] = words ? words(params(finding.params), finding.params || {}) : [finding.code, null];
              return el(
                "li",
                { class: `doctor-finding doctor-finding-${finding.severity}` },
                el("p", { class: "doctor-found", text: found }),
                fix ? el("p", { class: "doctor-fix", text: fix }) : null,
              );
            }),
          )
        : null,
      fresh && result.error ? el("p", { class: "detail", dir: "ltr", text: result.error }) : null,
      fresh && check === "dns" ? el("p", { class: "muted", text: t("doctor.dns_note") }) : null,
    );
  }

  function sqmSpeeds() {
    const result = state.results.bufferbloat;
    const sqm = result && (result.findings || []).find((f) => f.code === "sqm_suggested");
    if (sqm) {
      return t("router.speeds", { download: rate(sqm.params.download_kbps), upload: rate(sqm.params.upload_kbps) });
    }
    return t("router.speeds_unknown");
  }

  function renderRouters() {
    const list = routers();
    const select = $("router-brand");
    const signature = JSON.stringify(list.map((r) => [r.id, r.name]));
    if (select.dataset.signature !== signature) {
      select.replaceChildren(...list.map((r) => el("option", { value: r.id, text: r.name })));
      select.dataset.signature = signature;
    }
    if (select.value !== state.maker) {
      select.value = state.maker;
    }
    const maker = list.find((r) => r.id === state.maker) || list[list.length - 1];
    setText($("router-speeds"), sqmSpeeds());
    // Rebuilt only when they change: status updates arrive every second
    // while a game is optimized, and a rebuild loses the player's
    // selection and repeats the live region.
    rebuild($("router-steps"), [language(), maker.id], () =>
      maker.steps.split("\n").map((step) => el("li", { dir: "auto", text: step })),
    );
    show($("router-link"), maker.link);
    show($("router-link-error"), Boolean(state.linkError));
    setText($("router-link-error"), state.linkError ? errorText(state.linkError) : "");
  }

  function render() {
    $("doctor-run").disabled = state.running;
    setText($("doctor-run"), state.running ? t("doctor.running_button") : t("doctor.run"));
    let note = "";
    if (state.running) {
      note = t("doctor.running");
    } else if (state.finishedAt) {
      note = t("doctor.done", { time: time(state.finishedAt) });
    }
    setText($("doctor-note"), note);
    show($("doctor-error"), Boolean(state.error));
    setText($("doctor-error"), state.error ? errorText(state.error) : "");
    rebuild($("doctor-results"), [language(), state.running, [...state.started].sort(), state.results], () =>
      CHECKS.map(checkCard),
    );
    renderRouters();
  }

  // ---- Actions ----------------------------------------------------------------

  async function run() {
    if (state.running) {
      return;
    }
    state.running = true;
    state.own = true;
    state.error = null;
    state.finishedAt = null;
    state.results = {};
    state.started.clear();
    render();
    try {
      const report = await invoke("run_doctor");
      for (const result of report.results || []) {
        state.results[result.check] = result;
      }
      state.finishedAt = Date.now();
    } catch (error) {
      state.error = error;
    }
    state.running = false;
    state.own = false;
    state.started.clear();
    render();
  }

  /** A `doctor` event: this window's run, or one another window started. */
  function onEvent(progress) {
    if (progress.state === "started") {
      if (!state.running) {
        // The service can send a run's last events after its reply: a
        // late "started" for a check the reply already brought must not
        // wipe the results. A page that joins a run part way (reopened
        // during one, or another window's) has no result for it and
        // follows the run.
        if (progress.check !== CHECKS[0] && state.results[progress.check]) {
          return;
        }
        state.results = {};
        state.error = null;
        state.finishedAt = null;
        state.running = true;
      }
      state.started.add(progress.check);
    } else if (progress.state === "finished" && progress.result) {
      state.results[progress.result.check] = progress.result;
      state.started.delete(progress.result.check);
      if (progress.result.check === "bufferbloat") {
        state.running = false;
        state.finishedAt = Date.now();
      }
    }
    render();
  }

  function chooseMaker(id) {
    state.maker = id;
    state.linkError = null;
    renderRouters();
  }

  async function openGuide() {
    state.linkError = null;
    try {
      await invoke("open_router_guide", { maker: state.maker });
    } catch (error) {
      state.linkError = error;
    }
    renderRouters();
  }

  /** The service is out of reach: a run this window only followed through
   * events won't send its last one. (This window's own call fails by
   * itself.) */
  function connectionLost() {
    if (state.own || !state.running) {
      return;
    }
    state.running = false;
    state.started.clear();
    state.error = { code: "service_unavailable" };
    render();
  }

  return { render, run, onEvent, connectionLost, chooseMaker, openGuide };
};
