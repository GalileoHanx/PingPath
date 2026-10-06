// Headless smoke test of the window's page (client/ui/frontend), without
// Tauri or the service: a mock window.__TAURI__ answers the commands with
// canned data and sends the events, in English and in Arabic.
//
//   NODE_PATH=/opt/node-tools/node_modules node client/ui/tests/smoke.mjs
//
// It uses the globally installed playwright (Chromium). The page is served
// over loopback HTTP with the content security policy from tauri.conf.json,
// so an inline script or style the real window would refuse fails here too.
// Checks: the page starts without errors; Arabic sets dir="rtl" and
// lang="ar"; no raw translation key or unfilled {placeholder} is visible;
// nothing is wider than the window; and the main flows (Optimize on and off,
// the warning for games still being tested, another user's session, the
// banner when the service is unreachable) send the right commands.
// Screenshots go to client/ui/tests/screenshots/ (not committed).

import { createRequire } from "node:module";
import { createServer } from "node:http";
import { mkdirSync, readFileSync } from "node:fs";
import { dirname, extname, join, normalize, sep } from "node:path";
import { fileURLToPath } from "node:url";

// NODE_PATH applies to require(), not to ES module imports.
const require = createRequire(import.meta.url);
const { chromium } = require("playwright");

const HERE = dirname(fileURLToPath(import.meta.url));
const UI = join(HERE, "..");
const FRONTEND = join(UI, "frontend");
const SHOTS = join(HERE, "screenshots");
const WINDOW = { width: 1000, height: 760 }; // tauri.conf.json
const NARROW = { width: 720, height: 560 }; // its minimum size

const en = JSON.parse(readFileSync(join(FRONTEND, "i18n", "en.json"), "utf8"));
const ar = JSON.parse(readFileSync(join(FRONTEND, "i18n", "ar.json"), "utf8"));
const conf = JSON.parse(readFileSync(join(UI, "tauri.conf.json"), "utf8"));
const csp = Object.entries(conf.app.security.csp)
  .map(([directive, sources]) => `${directive} ${sources}`)
  .join("; ");

const failures = [];
function check(condition, message) {
  if (!condition) {
    failures.push(message);
    console.error(`FAIL ${message}`);
  }
}

// ---- The page, served as Tauri would ------------------------------------

const TYPES = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".json": "application/json" };

function serve() {
  const server = createServer((request, response) => {
    const path = new URL(request.url, "http://localhost").pathname;
    const file = normalize(join(FRONTEND, path === "/" ? "index.html" : decodeURIComponent(path)));
    if (!file.startsWith(FRONTEND + sep)) {
      response.writeHead(403).end();
      return;
    }
    try {
      const body = readFileSync(file);
      response.writeHead(200, {
        "Content-Type": `${TYPES[extname(file)] || "application/octet-stream"}; charset=utf-8`,
        "Content-Security-Policy": csp,
      });
      response.end(body);
    } catch {
      response.writeHead(404).end();
    }
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server)));
}

// ---- Canned data ----------------------------------------------------------

const NOW = Math.floor(Date.now() / 1000);
const REGIONS = [
  { id: "eu-central", name: "Europe (Frankfurt)" },
  { id: "eu-north", name: "Europe (Stockholm)" },
  { id: "us-east", name: "US East (Virginia)" },
];
const GAMES = [
  { id: "counter-strike-2", name: "Counter-Strike 2", status: "untested", routable: false, needs_warning: false, regions: REGIONS, test_definition: false },
  { id: "dota-2", name: "Dota 2", status: "untested", routable: false, needs_warning: false, regions: REGIONS, test_definition: false },
  { id: "echo-test", name: "Echo test", status: "testing", routable: true, needs_warning: true, regions: REGIONS.slice(0, 2), test_definition: true },
  { id: "rift-arena", name: "Rift Arena", status: "unsupported", routable: false, needs_warning: false, regions: REGIONS, test_definition: false },
  { id: "skyline-racers", name: "Skyline Racers", status: "supported", routable: true, needs_warning: false, regions: REGIONS, test_definition: false },
];

function measurement(game, region, needsWarning) {
  const regions = [
    { region: "eu-central", name: "Europe (Frankfurt)", direct: { rtt_ms: 118.4, loss_pct: 0.8 }, usual: true },
    { region: "eu-north", name: "Europe (Stockholm)", direct: { rtt_ms: 131.2, loss_pct: 0 }, usual: true },
    { region: "us-east", name: "US East (Virginia)", direct: { rtt_ms: 212.5, loss_pct: 0 }, usual: false },
  ].filter((r) => GAMES.find((g) => g.id === game).regions.some((x) => x.id === r.region));
  return {
    game,
    region,
    regions,
    direct: regions.find((r) => r.region === region).direct,
    best: { exit: "fra-1", entry: "bah-1", estimate_ms: 84.2 },
    worthwhile: true,
    needs_warning: needsWarning,
    measured_at_unix: NOW - 90,
  };
}

const ALL_NOTICES = [
  { id: 1, at_unix: NOW - 600, code: "fell_back", cause: "relays_unreachable" },
  { id: 2, at_unix: NOW - 540, code: "address_changed" },
  { id: 3, at_unix: NOW - 480, code: "failed_over", entry: "bah-2" },
  { id: 4, at_unix: NOW - 420, code: "failed_over", entry: null },
  { id: 5, at_unix: NOW - 360, code: "not_better", direct_ms: 64.2, optimized_ms: 66.9 },
  { id: 6, at_unix: NOW - 300, code: "direct_unknown" },
  { id: 7, at_unix: NOW - 240, code: "refused", refusal: { reason: "not_routable", game: "Dota 2", status: "untested" } },
  { id: 8, at_unix: NOW - 180, code: "game_traffic_blocked", packets: 1240 },
  { id: 9, at_unix: NOW - 120, code: "error", message: "the adapter could not be created" },
  { id: 10, at_unix: NOW - 60, code: "fell_back", cause: "suspended" },
];

function optimizedStatus(owner, notices) {
  return {
    mode: "optimized",
    game: "skyline-racers",
    region: "eu-central",
    measurement: measurement("skyline-racers", "eu-central", false),
    path: { exit: "fra-1", entry: "bah-1", since_unix: NOW - 300, optimized_ms: 86.4, direct_ms: 118.4 },
    carried: 15234,
    blocked: [],
    notices,
    owner,
  };
}

function readyStatus(game, notices = []) {
  const testing = game === "echo-test";
  return {
    mode: "ready",
    game,
    region: "eu-central",
    measurement: measurement(game, "eu-central", testing),
    path: null,
    carried: 0,
    blocked: [],
    notices,
    owner: null,
  };
}

function samples() {
  // Two minutes: direct every two seconds, then through PingPath every second.
  const now = Date.now();
  const out = [];
  for (let s = 118; s > 60; s -= 2) {
    out.push({ at_unix_ms: now - s * 1000, optimized: false, ping_ms: 116 + 6 * Math.sin(s / 3), jitter_ms: null, loss_pct: s % 14 === 0 ? 3 : 0 });
  }
  for (let s = 58; s >= 1; s -= 1) {
    out.push({ at_unix_ms: now - s * 1000, optimized: true, ping_ms: 85 + 3 * Math.sin(s / 2), jitter_ms: 2.5 + Math.abs(Math.sin(s)), loss_pct: s === 20 ? 2 : 0 });
  }
  return out;
}

const CONNECTED = { state: "connected", reason: null, detail: null, service_version: "0.1.0", retry_in_ms: null };
const UNAVAILABLE = {
  state: "unavailable",
  reason: "not_running",
  detail: "The system cannot find the file specified. (os error 2)",
  service_version: null,
  retry_in_ms: 4000,
};
const SETTINGS = { control_url: "https://control.pingpath.net", verbose_logging: false, min_improvement_ms: 10, min_improvement_pct: 10 };

// ---- The mock, installed in the page before its scripts run ------------

function installMock(scenario) {
  const calls = [];
  const listeners = {};
  const violations = [];
  document.addEventListener("securitypolicyviolation", (e) => violations.push(`${e.violatedDirective}: ${e.blockedURI}`));
  const copy = (value) => (value === undefined ? null : JSON.parse(JSON.stringify(value)));
  const emit = (name, payload) => (listeners[name] || []).forEach((handler) => handler({ event: name, payload: copy(payload) }));
  const status = () => scenario.status;
  const language = (preference) => ({
    preference,
    language: preference === "system" ? (scenario.system.startsWith("ar") ? "ar" : "en") : preference,
    system: scenario.system,
  });
  let preference = scenario.preference;
  const connected = () => scenario.connection.state === "connected";
  const unavailable = { code: "service_unavailable", message: "not connected to the PingPath service" };
  const handlers = {
    app_info: () => ({ version: "0.1.0" }),
    get_language: () => language(preference),
    set_language: ({ preference: chosen }) => {
      preference = chosen;
      return language(chosen);
    },
    acknowledged_games: () => scenario.acks,
    acknowledge_game: ({ game }) => {
      scenario.acks.push(game);
      return null;
    },
    connection: () => scenario.connection,
    reconnect: () => null,
    games: () => scenario.games,
    status,
    settings: () => scenario.settings,
    set_settings: ({ change }) => {
      for (const [key, value] of Object.entries(change)) {
        if (value !== null && value !== undefined) {
          scenario.settings[key] = value;
        }
      }
      return scenario.settings;
    },
    measure: ({ game, region }) => {
      scenario.status = { ...scenario.readyFor(game), region };
      emit("status", scenario.status);
      return scenario.status.measurement;
    },
    enable: ({ acknowledged_warning }) => {
      const s = scenario.status;
      if (s.measurement.needs_warning && !acknowledged_warning) {
        throw { code: "warning_not_acknowledged", message: "acknowledge the warning first" };
      }
      scenario.status = { ...scenario.optimized, game: s.game, region: s.region, measurement: s.measurement, owner: "you" };
      emit("status", scenario.status);
      return scenario.status;
    },
    disable: () => {
      scenario.status = { ...scenario.status, mode: "ready", path: null, carried: 0 };
      emit("status", scenario.status);
      return scenario.status;
    },
    dismiss: ({ id }) => {
      scenario.status = { ...scenario.status, notices: scenario.status.notices.filter((n) => n.id !== id) };
      emit("status", scenario.status);
      return null;
    },
    copy_report: () => ({ path: "C:\\Users\\player\\Documents\\PingPath\\pingpath-report-20261005-101500.md", copied: true }),
  };
  const needsService = new Set(["games", "status", "settings", "set_settings", "measure", "enable", "disable", "dismiss"]);
  window.__mock = { calls, emit, violations, scenario };
  window.__TAURI__ = {
    core: {
      invoke: (command, args) =>
        new Promise((resolve, reject) => {
          calls.push([command, copy(args || {})]);
          const handler = handlers[command];
          if (!handler) {
            reject(`command ${command} not found`);
            return;
          }
          if (needsService.has(command) && !connected()) {
            reject(copy(unavailable));
            return;
          }
          // Answer a moment later, as the real window does.
          setTimeout(() => {
            try {
              resolve(copy(handler(args || {})));
            } catch (error) {
              reject(copy(error));
            }
          }, 5);
        }),
    },
    event: {
      listen: (name, handler) => {
        (listeners[name] = listeners[name] || []).push(handler);
        if (name === "sample") {
          setTimeout(() => scenario.samples.forEach((s) => emit("sample", s)), 0);
        }
        return Promise.resolve(() => {});
      },
    },
  };
}

// The page gets the scenario as plain data, plus the functions that build
// statuses (the mock's `measure` needs them).
function pageScript(base) {
  return [
    `const NOW = ${NOW};`,
    `const GAMES = ${JSON.stringify(GAMES)};`,
    measurement.toString(),
    readyStatus.toString(),
    `(${installMock.toString()})(Object.assign(${JSON.stringify(base)}, { readyFor: (game) => readyStatus(game) }));`,
  ].join("\n");
}

function scenario(overrides) {
  return {
    system: "en-US",
    preference: "system",
    connection: CONNECTED,
    games: GAMES,
    status: readyStatus("skyline-racers"),
    optimized: optimizedStatus("you", []),
    settings: { ...SETTINGS },
    acks: [],
    samples: [],
    ...overrides,
  };
}

// ---- Checks on a loaded page ------------------------------------------------

async function visibleTexts(page) {
  return page.evaluate(() => {
    const texts = [document.title, document.body.innerText];
    for (const node of document.querySelectorAll("[aria-label], [title]")) {
      if (node.closest("[hidden]")) {
        continue;
      }
      texts.push(node.getAttribute("aria-label") || "", node.getAttribute("title") || "");
    }
    return texts.join("\n");
  });
}

async function checkPage(page, name, lang) {
  const html = await page.evaluate(() => ({ dir: document.documentElement.dir, lang: document.documentElement.lang }));
  check(html.lang === lang, `${name}: <html lang> is ${html.lang}, expected ${lang}`);
  check(html.dir === (lang === "ar" ? "rtl" : "ltr"), `${name}: <html dir> is ${html.dir}`);
  const text = await visibleTexts(page);
  for (const key of Object.keys(en)) {
    if (text.includes(key)) {
      check(false, `${name}: raw key ${key} is visible`);
    }
  }
  const leaked = text.match(/\b(null|undefined|NaN|\[object Object\])\b/);
  check(!leaked, `${name}: "${leaked && leaked[0]}" is visible`);
  const placeholder = text.match(/\{[a-z_]+\}/);
  check(!placeholder, `${name}: unfilled placeholder ${placeholder && placeholder[0]}`);
  const dict = lang === "ar" ? ar : en;
  const heading = await page.evaluate(() => document.querySelector("[data-i18n='game.heading']").textContent);
  check(heading === dict["game.heading"], `${name}: game heading is "${heading}"`);
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  check(overflow <= 0, `${name}: the page is ${overflow}px wider than the window`);
  const violations = await page.evaluate(() => window.__mock.violations);
  check(violations.length === 0, `${name}: content security policy violations: ${violations.join(", ")}`);
}

async function open(browser, base, url, viewport = WINDOW) {
  const page = await browser.newPage({ viewport, deviceScaleFactor: 1 });
  const errors = [];
  page.on("pageerror", (error) => errors.push(String(error)));
  page.on("console", (message) => {
    if (message.type() === "error") {
      errors.push(message.text());
    }
  });
  await page.addInitScript({ content: pageScript(base) });
  await page.goto(url);
  await page.waitForFunction(() => document.querySelector("#connection-pill").textContent !== "");
  await page.waitForTimeout(150);
  page.errors = errors;
  return page;
}

async function shot(page, file) {
  await page.screenshot({ path: join(SHOTS, file), fullPage: true });
}

async function calls(page) {
  return page.evaluate(() => window.__mock.calls);
}

async function setLanguage(page, language) {
  if (await page.isVisible("#nav-settings")) {
    await page.click("#nav-settings");
  }
  await page.click(`[data-language='${language}']`);
  await page.waitForFunction((lang) => document.documentElement.lang === lang, language === "system" ? "en" : language);
  await page.waitForTimeout(100);
}

// ---- Scenarios ------------------------------------------------------------

async function run() {
  mkdirSync(SHOTS, { recursive: true });
  const server = await serve();
  const url = `http://127.0.0.1:${server.address().port}/`;
  const browser = await chromium.launch();
  try {
    // 1. Optimized, with live samples and a few notices: English, then
    //    Arabic chosen in Settings.
    {
      const base = scenario({
        status: optimizedStatus("you", [ALL_NOTICES[2], ALL_NOTICES[7]]),
        samples: samples(),
      });
      const page = await open(browser, base, url);
      await checkPage(page, "main en", "en");
      const toggle = await page.getAttribute("#optimize", "aria-checked");
      check(toggle === "true", "main en: the Optimize switch is on");
      check(await page.isVisible("#session"), "main en: the session details show");
      check((await page.textContent("#session-packets")).includes("15,234"), "main en: the packet counter shows 15,234");
      check(!(await page.isVisible("#graph-empty")), "main en: the graph has samples");
      check(await page.isDisabled("#measure"), "main en: Measure is off while optimized");
      await shot(page, "main-en.png");

      await page.click("#nav-settings");
      await page.waitForTimeout(100);
      await checkPage(page, "settings en", "en");
      await shot(page, "settings-en.png");

      await setLanguage(page, "ar");
      await checkPage(page, "settings ar", "ar");
      await shot(page, "settings-ar.png");
      const chosen = (await calls(page)).filter(([c]) => c === "set_language").map(([, a]) => a.preference);
      check(chosen.join() === "ar", `set_language sent ${chosen.join()}`);

      await page.click("#nav-back");
      await page.waitForTimeout(150);
      await checkPage(page, "main ar", "ar");
      await shot(page, "main-ar.png");

      // Turning Optimize off.
      await page.click("#optimize");
      await page.waitForFunction(() => document.querySelector("#optimize").getAttribute("aria-checked") === "false");
      check((await calls(page)).some(([c]) => c === "disable"), "main ar: the switch sends disable");
      check(page.errors.length === 0, `main: page errors: ${page.errors.join(" | ")}`);
      await page.close();
    }

    // 2. Every kind of notice, in both languages.
    for (const lang of ["en", "ar"]) {
      const base = scenario({ preference: lang, status: readyStatus("skyline-racers", ALL_NOTICES) });
      const page = await open(browser, base, url);
      await checkPage(page, `notices ${lang}`, lang);
      const count = await page.locator(".notice").count();
      check(count === ALL_NOTICES.length, `notices ${lang}: ${count} notices shown`);
      await shot(page, `notices-${lang}.png`);
      await page.click(".notice .button");
      await page.waitForFunction((n) => document.querySelectorAll(".notice").length === n - 1, ALL_NOTICES.length);
      const dismissed = (await calls(page)).find(([c]) => c === "dismiss");
      check(dismissed && dismissed[1].id === 10, `notices ${lang}: dismiss sent the newest notice's id`);
      check(page.errors.length === 0, `notices ${lang}: page errors: ${page.errors.join(" | ")}`);
      await page.close();
    }

    // 3. The service can't be reached.
    for (const lang of ["en", "ar"]) {
      const base = scenario({ preference: lang, connection: UNAVAILABLE, status: null });
      const page = await open(browser, base, url);
      await checkPage(page, `banner ${lang}`, lang);
      check(await page.isVisible("#banner"), `banner ${lang}: the banner shows`);
      check(await page.isDisabled("#optimize"), `banner ${lang}: Optimize is off`);
      await page.click("#banner-retry-button");
      await page.waitForTimeout(50);
      check((await calls(page)).some(([c]) => c === "reconnect"), `banner ${lang}: Try again sends reconnect`);
      await shot(page, `banner-${lang}.png`);
      await page.close();
    }

    // 4. A game still being tested: measure, then the one-time warning.
    for (const lang of ["en", "ar"]) {
      const base = scenario({ preference: lang, status: { ...readyStatus("skyline-racers"), mode: "idle", measurement: null, game: null, region: null } });
      const page = await open(browser, base, url);
      await page.click("[data-id='echo-test']");
      await page.click("#measure");
      await page.waitForFunction(() => !document.querySelector("#optimize").disabled);
      await checkPage(page, `measured ${lang}`, lang);
      await page.click("#optimize");
      await page.waitForSelector("#warning-dialog[open]");
      await checkPage(page, `warning ${lang}`, lang);
      await shot(page, `warning-${lang}.png`);
      await page.click("#warning-accept");
      await page.waitForFunction(() => document.querySelector("#optimize").getAttribute("aria-checked") === "true");
      const sent = await calls(page);
      const measured = sent.find(([c]) => c === "measure");
      check(measured && measured[1].game === "echo-test" && measured[1].region === "eu-central", `warning ${lang}: measure sent ${JSON.stringify(measured)}`);
      check(sent.some(([c, a]) => c === "acknowledge_game" && a.game === "echo-test"), `warning ${lang}: the acknowledgement is remembered`);
      check(sent.some(([c, a]) => c === "enable" && a.acknowledged_warning === true), `warning ${lang}: enable sent acknowledged_warning`);
      // Off and on again: no second question.
      await page.click("#optimize");
      await page.waitForFunction(() => document.querySelector("#optimize").getAttribute("aria-checked") === "false");
      await page.click("#optimize");
      await page.waitForFunction(() => document.querySelector("#optimize").getAttribute("aria-checked") === "true");
      check(!(await page.isVisible("#warning-dialog")), `warning ${lang}: asked only once`);
      check(page.errors.length === 0, `warning ${lang}: page errors: ${page.errors.join(" | ")}`);
      await page.close();
    }

    // 5. Another user of this PC turned Optimize on, and no game traffic
    //    went through yet; the narrowest window.
    for (const lang of ["en", "ar"]) {
      const status = optimizedStatus("another_user", [ALL_NOTICES[0]]);
      status.carried = 0;
      // In Arabic, following an Arabic Windows (ar-SA): its digits too.
      const language = lang === "ar" ? { preference: "system", system: "ar-SA" } : { preference: "en" };
      const base = scenario({ ...language, status });
      const page = await open(browser, base, url, NARROW);
      await checkPage(page, `another user ${lang}`, lang);
      if (lang === "ar") {
        const packets = await page.textContent("#session-packets");
        check(/^[\u0660-\u0669]+$/.test(packets), `another user ar: ar-SA digits in the counter (${packets})`);
      }
      check(await page.isDisabled("#optimize"), `another user ${lang}: the switch is disabled`);
      check(await page.isVisible("#optimize-owner"), `another user ${lang}: the note says why`);
      check(!(await page.isVisible("#session-hint")), `another user ${lang}: no rejoin hint for someone else's session`);
      check(await page.isDisabled(".notice .button"), `another user ${lang}: their notices can't be dismissed`);
      await shot(page, `another-user-${lang}.png`);
      await page.close();
    }

    // 6. Today's built-in catalogue: every game untested, so nothing can
    //    be picked or measured.
    for (const lang of ["en", "ar"]) {
      const games = GAMES.filter((g) => g.status === "untested");
      const status = { ...readyStatus("skyline-racers"), mode: "idle", game: null, region: null, measurement: null };
      const page = await open(browser, scenario({ preference: lang, games, status }), url);
      await checkPage(page, `untested ${lang}`, lang);
      check((await page.locator(".choice:disabled").count()) === games.length, `untested ${lang}: every game is greyed`);
      check((await page.locator(".choice[aria-checked='true']").count()) === 0, `untested ${lang}: none is selected`);
      check(!(await page.isVisible("#region-section")), `untested ${lang}: no region picker`);
      check(await page.isVisible("#game-none-routable"), `untested ${lang}: the page says why`);
      check(await page.isDisabled("#optimize"), `untested ${lang}: Optimize is off`);
      await shot(page, `untested-${lang}.png`);
      check(page.errors.length === 0, `untested ${lang}: page errors: ${page.errors.join(" | ")}`);
      await page.close();
    }

    // 7. The player's own session, but no game packets yet: the hint.
    {
      const status = optimizedStatus("you", []);
      status.carried = 0;
      const page = await open(browser, scenario({ preference: "ar", status }), url);
      check(await page.isVisible("#session-hint"), "no traffic ar: the rejoin hint shows");
      check(!(await page.isDisabled("#optimize")), "no traffic ar: the owner can turn it off");
      await page.close();
    }

    // 8. Settings: thresholds typed with Arabic digits, the report.
    {
      const base = scenario({ preference: "ar" });
      const page = await open(browser, base, url);
      await page.click("#nav-settings");
      await page.fill("#min-ms", "١٥");
      await page.fill("#min-pct", "١٢٫٥");
      await page.click("#thresholds-save");
      await page.waitForFunction(() => document.querySelector("#thresholds-status").textContent !== "");
      const change = (await calls(page)).find(([c]) => c === "set_settings");
      check(
        change && change[1].change.min_improvement_ms === 15 && change[1].change.min_improvement_pct === 12.5,
        `settings ar: thresholds sent as ${JSON.stringify(change)}`,
      );
      await page.fill("#min-ms", "5000");
      await page.click("#thresholds-save");
      await page.waitForTimeout(50);
      const saves = (await calls(page)).filter(([c]) => c === "set_settings").length;
      check(saves === 1, "settings ar: out-of-range thresholds are not sent");
      check(
        (await page.getAttribute("#min-ms", "aria-invalid")) === "true",
        "settings ar: the out-of-range field is marked",
      );
      await shot(page, "settings-invalid-ar.png");
      await page.click("#verbose");
      await page.waitForFunction(() => document.querySelector("#verbose").getAttribute("aria-checked") === "true");
      await page.click("#report");
      await page.waitForFunction(() => document.querySelector("#report-status").textContent !== "");
      await checkPage(page, "settings ar after actions", "ar");
      await shot(page, "settings-actions-ar.png");
      check(page.errors.length === 0, `settings ar: page errors: ${page.errors.join(" | ")}`);
      await page.close();
    }
  } finally {
    await browser.close();
    server.close();
  }
}

try {
  await run();
} catch (error) {
  failures.push(String(error && error.stack ? error.stack : error));
  console.error(error);
}
if (failures.length > 0) {
  console.error(`smoke: ${failures.length} failure(s)`);
  process.exit(1);
}
console.log(`smoke: passed; screenshots in ${SHOTS}`);
