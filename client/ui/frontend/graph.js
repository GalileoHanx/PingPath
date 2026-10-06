// The live graph: ping, jitter and packet loss over the last two minutes,
// drawn on a canvas. The service sends a direct sample every two seconds
// while a game is measured but not optimized, and one sample a second
// through PingPath while optimized (client/core/src/engine/types.rs,
// `Sample`): the two are drawn in different styles (dashed and solid), so
// the switch from one to the other shows.
//
// Time runs towards the inline end: "now" is on the right in English and on
// the left in Arabic, with the axis labels on the start side, so the graph
// mirrors with the rest of the layout. Colours come from the page's CSS
// variables (style.css), so the graph follows light and dark mode; numbers
// and words come from app.js, so they follow the language.

"use strict";

(function () {
  const WINDOW_MS = 120 * 1000;
  // A gap longer than this many sample intervals breaks the line.
  const GAP_INTERVALS = 2.5;
  const INTERVAL_MS = { direct: 2000, optimized: 1000 };
  const STEPS = [5, 10, 20, 25, 50, 100, 200, 250, 500, 1000, 2000];

  class LiveGraph {
    constructor(canvas) {
      this.canvas = canvas;
      this.samples = [];
      this.format = {
        number: (value) => String(Math.round(value)),
        percent: (value) => `${Math.round(value)}%`,
        ago: (seconds) => `-${seconds}s`,
        now: "now",
        msUnit: "ms",
        rtl: false,
      };
      if (typeof ResizeObserver === "function") {
        new ResizeObserver(() => this.draw()).observe(canvas);
      }
    }

    /** Words, number formats and direction from the page's language. */
    setFormat(format) {
      Object.assign(this.format, format);
      this.draw();
    }

    /** One `Sample` from the service. */
    push(sample) {
      if (!sample || typeof sample.at_unix_ms !== "number") {
        return;
      }
      this.samples.push(sample);
      if (this.samples.length > 1 && sample.at_unix_ms < this.samples[this.samples.length - 2].at_unix_ms) {
        this.samples.sort((a, b) => a.at_unix_ms - b.at_unix_ms);
      }
      this.prune(Date.now());
      this.draw();
    }

    clear() {
      this.samples = [];
      this.draw();
    }

    hasData() {
      return this.prune(Date.now()) > 0;
    }

    prune(now) {
      const oldest = now - WINDOW_MS - 5000;
      let drop = 0;
      while (drop < this.samples.length && this.samples[drop].at_unix_ms < oldest) {
        drop += 1;
      }
      if (drop > 0) {
        this.samples.splice(0, drop);
      }
      return this.samples.length;
    }

    colors() {
      const style = getComputedStyle(this.canvas);
      const read = (name, fallback) => style.getPropertyValue(name).trim() || fallback;
      return {
        direct: read("--graph-direct", "#6b7c86"),
        optimized: read("--graph-optimized", "#0d97a3"),
        jitter: read("--graph-jitter", "#d58300"),
        loss: read("--graph-loss", "rgba(214,48,39,0.55)"),
        grid: read("--graph-grid", "#e3eaed"),
        text: read("--graph-text", "#6b7a81"),
        font: style.fontFamily || "sans-serif",
      };
    }

    draw() {
      const canvas = this.canvas;
      const width = canvas.clientWidth;
      const height = canvas.clientHeight;
      if (width === 0 || height === 0) {
        return;
      }
      const ratio = window.devicePixelRatio || 1;
      const pixelWidth = Math.round(width * ratio);
      const pixelHeight = Math.round(height * ratio);
      if (canvas.width !== pixelWidth || canvas.height !== pixelHeight) {
        canvas.width = pixelWidth;
        canvas.height = pixelHeight;
      }
      const ctx = canvas.getContext("2d");
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
      ctx.clearRect(0, 0, width, height);

      const now = Date.now();
      // Nothing to show: the page says so over an empty canvas.
      if (this.prune(now) === 0) {
        return;
      }
      const f = this.format;
      const c = this.colors();
      const rtl = Boolean(f.rtl);

      // Plot area: an axis gutter on the start side, labels below.
      const gutter = 52;
      const top = 28;
      const bottom = height - 22;
      const plotStart = rtl ? 8 : gutter;
      const plotEnd = rtl ? width - gutter : width - 8;
      const plotWidth = Math.max(10, plotEnd - plotStart);
      const lossHeight = Math.round((bottom - top) * 0.2);
      const lossTop = bottom - lossHeight;
      const pingBottom = lossTop - 14;
      const pingHeight = Math.max(10, pingBottom - top);

      const x = (t) => {
        const share = (t - (now - WINDOW_MS)) / WINDOW_MS;
        return rtl ? plotEnd - share * plotWidth : plotStart + share * plotWidth;
      };

      // The ping scale: round steps, at least 50 ms.
      let highest = 0;
      for (const s of this.samples) {
        if (typeof s.ping_ms === "number") {
          highest = Math.max(highest, s.ping_ms);
        }
        if (typeof s.jitter_ms === "number") {
          highest = Math.max(highest, s.jitter_ms);
        }
      }
      highest = Math.max(50, highest * 1.1);
      const step = STEPS.find((candidate) => highest / candidate <= 4) || STEPS[STEPS.length - 1];
      const topMs = Math.ceil(highest / step) * step;
      const y = (ms) => pingBottom - (Math.min(ms, topMs) / topMs) * pingHeight;

      ctx.font = `12px ${c.font}`;
      ctx.lineWidth = 1;
      ctx.textBaseline = "middle";
      const labelX = rtl ? plotEnd + 8 : plotStart - 8;
      ctx.textAlign = rtl ? "left" : "right";
      ctx.direction = rtl ? "rtl" : "ltr";

      // Grid lines and ping labels; 0 ms goes unlabelled, as it sits next
      // to the loss strip's 100 %.
      ctx.strokeStyle = c.grid;
      ctx.fillStyle = c.text;
      for (let ms = 0; ms <= topMs; ms += step) {
        const lineY = Math.round(y(ms)) + 0.5;
        ctx.beginPath();
        ctx.moveTo(plotStart, lineY);
        ctx.lineTo(plotEnd, lineY);
        ctx.stroke();
        if (ms > 0) {
          ctx.fillText(f.number(ms), labelX, lineY);
        }
      }
      ctx.fillText(f.msUnit, labelX, 10);

      // The loss strip: 0 to 100 % from its bottom.
      ctx.textBaseline = "middle";
      ctx.beginPath();
      ctx.moveTo(plotStart, bottom + 0.5);
      ctx.lineTo(plotEnd, bottom + 0.5);
      ctx.moveTo(plotStart, lossTop + 0.5);
      ctx.lineTo(plotEnd, lossTop + 0.5);
      ctx.stroke();
      ctx.fillText(f.percent(100), labelX, lossTop);
      ctx.fillText(f.percent(0), labelX, bottom);

      // Time labels every 30 seconds.
      ctx.textBaseline = "top";
      ctx.textAlign = "center";
      for (let seconds = 0; seconds <= 120; seconds += 30) {
        const labelTime = now - seconds * 1000;
        let at = x(labelTime);
        const text = seconds === 0 ? f.now : f.ago(seconds);
        const half = ctx.measureText(text).width / 2;
        at = Math.min(Math.max(at, plotStart + half), plotEnd - half);
        ctx.fillText(text, at, bottom + 6);
      }

      ctx.save();
      ctx.beginPath();
      ctx.rect(plotStart, 0, plotWidth, height);
      ctx.clip();

      // Loss bars.
      ctx.fillStyle = c.loss;
      for (const s of this.samples) {
        if (!(s.loss_pct > 0)) {
          continue;
        }
        const kind = s.optimized ? "optimized" : "direct";
        const barWidth = Math.max(2, (INTERVAL_MS[kind] / WINDOW_MS) * plotWidth - 1);
        const barHeight = (Math.min(100, s.loss_pct) / 100) * lossHeight;
        const centre = x(s.at_unix_ms);
        ctx.fillRect(centre - barWidth / 2, bottom - barHeight, barWidth, barHeight);
      }

      // Lines: ping direct (dashed), ping through PingPath (solid), and the
      // jitter through PingPath (dotted); the direct samples carry none.
      const line = (kind, value, color, width, dash) => {
        ctx.strokeStyle = color;
        ctx.lineWidth = width;
        ctx.setLineDash(dash);
        ctx.lineJoin = "round";
        ctx.lineCap = "round";
        ctx.beginPath();
        let last = null;
        let lastPoint = null;
        for (const s of this.samples) {
          if (s.optimized !== (kind === "optimized")) {
            continue;
          }
          const v = value(s);
          if (typeof v !== "number") {
            last = null;
            continue;
          }
          const px = x(s.at_unix_ms);
          const py = y(v);
          const gap = last !== null && s.at_unix_ms - last > GAP_INTERVALS * INTERVAL_MS[kind];
          if (last === null || gap) {
            ctx.moveTo(px, py);
          } else {
            ctx.lineTo(px, py);
          }
          last = s.at_unix_ms;
          lastPoint = [px, py];
        }
        ctx.stroke();
        ctx.setLineDash([]);
        return lastPoint;
      };
      line("optimized", (s) => s.jitter_ms, c.jitter, 1.5, [2, 3]);
      const direct = line("direct", (s) => s.ping_ms, c.direct, 2, [6, 4]);
      const optimized = line("optimized", (s) => s.ping_ms, c.optimized, 2.5, []);
      for (const [point, color] of [[direct, c.direct], [optimized, c.optimized]]) {
        if (point) {
          ctx.fillStyle = color;
          ctx.beginPath();
          ctx.arc(point[0], point[1], 3.5, 0, Math.PI * 2);
          ctx.fill();
        }
      }
      ctx.restore();
    }
  }

  window.PingPathGraph = { LiveGraph, WINDOW_MS };
})();
