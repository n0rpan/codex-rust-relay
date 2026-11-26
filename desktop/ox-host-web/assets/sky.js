// The living header horizon. Time of day, sun and moon come from this phone's own clock (a
// fixed 06:00 to 18:00 day, no location). Weather comes from the host, which derives a
// city-level place from the laptop's timezone and asks MET Norway (CC BY 4.0).
const W = 166;
const H = 42;
const STARS = [
  [24, 9], [62, 4], [104, 15], [132, 7], [40, 21], [150, 12],
  [88, 8], [12, 14], [118, 25], [70, 23], [160, 5], [141, 23],
];
const HILLS = [0, 2, 4, 6, 8, 6, 4, 2, 4, 6, 8, 10, 12, 10, 8, 6, 4, 2, 4, 6, 4, 2];
const CLOUD_SMALL = [[6, 0, 10], [2, 2, 18], [0, 4, 26]];
const CLOUD_LARGE = [[8, 0, 12], [4, 2, 20], [0, 4, 30], [2, 6, 26]];
const LABELS = {
  clear: "Clear", partly_cloudy: "Partly cloudy", cloudy: "Cloudy", rain: "Rain",
  snow: "Snow", sleet: "Sleet", fog: "Fog", thunder: "Thunderstorm",
};
const WEATHER_EVERY = 30 * 60 * 1000;

/** Moon phase as a fraction of the synodic month: 0 new, 0.5 full. */
export function moonPhase(date) {
  const synodic = 29.530588853;
  const jd = date.getTime() / 86400000 + 2440587.5;
  return ((((jd - 2451550.26) % synodic) + synodic) % synodic) / synodic;
}

/** The sky for a local clock reading, without any location. */
export function skyAt(date, condition = null) {
  const minutes = date.getHours() * 60 + date.getMinutes();
  const ha = minutes / 4 - 180;
  const elevation = (Math.asin(Math.cos((ha * Math.PI) / 180)) * 180) / Math.PI;
  const phase =
    elevation < -6 ? "night" : elevation < 3 ? (ha < 0 ? "dawn" : "dusk") : "day";
  const moon = phase === "night";
  let x;
  let height;
  if (moon) {
    const night = Math.min(0.96, Math.max(0.04, (((ha + 360) % 360) - 90) / 180));
    x = night;
    height = 0.25 + 0.6 * Math.sin(Math.PI * night);
  } else {
    x = Math.min(0.97, Math.max(0.03, 0.5 + ha / 180));
    height = Math.min(1, Math.max(-0.3, elevation / 90));
  }
  return { phase, moon, x, height, moonPhase: moonPhase(date), condition: condition || "clear" };
}

const hex = (value) => {
  const m = /^#([0-9a-f]{6})$/i.exec(value.trim());
  return m ? parseInt(m[1], 16) : 0;
};
const mix = (a, b, percent) => {
  const ch = (s) =>
    Math.floor((((a >> s) & 255) * (100 - percent) + ((b >> s) & 255) * percent + 50) / 100);
  return (ch(16) << 16) | (ch(8) << 8) | ch(0);
};
const css = (color) => "#" + color.toString(16).padStart(6, "0");

function palette() {
  const style = getComputedStyle(document.documentElement);
  const get = (name) => hex(style.getPropertyValue(name));
  return {
    nav: get("--nav"),
    accent: get("--accent"),
    warning: get("--warning"),
    error: get("--error"),
    fg: get("--ink"),
    muted: get("--muted"),
    light: document.documentElement.dataset.phoneTheme === "light",
  };
}

const animated = (condition) => condition !== "clear";

export function paint(ctx, p, sky, frame) {
  const rect = (x, y, w, h, color) => {
    ctx.fillStyle = css(color);
    ctx.fillRect(x, y, w, h);
  };
  const ink = (dark, light) => (p.light ? light : dark);
  const overcast = !["clear", "partly_cloudy"].includes(sky.condition);
  ctx.clearRect(0, 0, W, H);

  const stars =
    sky.phase === "night"
      ? { clear: 12, partly_cloudy: 6 }[sky.condition] || 0
      : sky.phase !== "day" && ["clear", "partly_cloudy"].includes(sky.condition)
        ? 3
        : 0;
  STARS.slice(0, stars).forEach(([x, y], i) => {
    const dim = frame > 0 && (Math.floor(frame / 6) + i) % 9 === 0;
    rect(W - x, y, 2, 2, mix(p.nav, p.fg, dim ? 20 : ink(52, 46)));
  });

  const x0 = W - 150;
  const x1 = W - 20;
  const r = sky.moon ? 8 : 12;
  const cx = x0 + sky.x * (x1 - x0);
  const cy = H - 14 - sky.height * (H - 14 - r - 2);
  if (sky.moon) {
    const lit = mix(p.nav, p.fg, ink(82, 58));
    const earth = mix(p.nav, p.fg, ink(14, 12));
    const k = Math.cos(2 * Math.PI * sky.moonPhase);
    const even = (v) => Math.round(v / 2) * 2;
    for (let dy = -r; dy < r; dy += 2) {
      const mid = dy + 1;
      const half = even(Math.sqrt(Math.max(0, r * r - mid * mid)));
      rect(cx - half, cy + dy, half * 2, 2, earth);
      const [from, to] = sky.moonPhase <= 0.5 ? [half * k, half] : [-half, -half * k];
      const [a, c] = [even(from), even(to)];
      if (c > a) rect(cx + a, cy + dy, c - a, 2, lit);
    }
  } else {
    const base = sky.phase === "day" ? p.warning : mix(p.warning, p.error, 38);
    let color = mix(p.nav, base, ink(76, 57));
    if (overcast) color = mix(color, p.nav, 55);
    let row = 0;
    for (let dy = -r; dy < r; dy += 2, row++) {
      const mid = dy + 1;
      const half = Math.round(Math.sqrt(Math.max(0, r * r - mid * mid)) / 2) * 2;
      if (!(dy > r * 0.15 && row % 2 === 1)) rect(cx - half, cy + dy, half * 2, 2, color);
    }
  }

  const [clouds, amount] = {
    partly_cloudy: [[[40, 8]], 26],
    cloudy: [[[10, 6], [70, 16], [130, 4]], 36],
    rain: [[[10, 4], [60, 10], [110, 3], [150, 12]], 46],
  }[
    ["snow", "sleet", "thunder"].includes(sky.condition) ? "rain" : sky.condition
  ] || [[], 0];
  const span = x1 - x0 + 60;
  clouds.forEach(([seed, y], i) => {
    const shape = i % 2 === 0 ? CLOUD_SMALL : CLOUD_LARGE;
    const x = Math.floor((x0 - 30 + ((seed + frame) % span)) / 2) * 2;
    for (const [dx, dy, w] of shape) rect(x + dx, y + dy, w, 2, mix(p.nav, p.muted, amount));
  });

  HILLS.forEach((rise, i) => {
    const x = W - 132 + i * 6;
    rect(x, H - 10 - rise, 6, rise + 10, mix(p.nav, p.accent, p.light ? 11 : 19));
    rect(x, H - 10 - rise, 6, 2, mix(p.nav, p.accent, 40));
  });
  rect(0, H - 2, W, 2, mix(p.nav, p.accent, 43));

  const fall = Math.max(8, H - 26);
  const down = (y) => Math.floor(y / 2) * 2;
  if (["rain", "thunder", "sleet"].includes(sky.condition)) {
    for (let i = 0; i < 14; i++) {
      if (sky.condition === "sleet" && i % 2) continue;
      const x = W - 150 + i * 10 + ((i * 7) % 4) * 2;
      rect(x, down(14 + ((frame * 3 + i * 13) % fall)), 2, 4, mix(p.nav, p.accent, 62));
    }
  }
  if (["snow", "sleet"].includes(sky.condition)) {
    for (let i = 0; i < 12; i++) {
      if (sky.condition === "sleet" && i % 2 === 0) continue;
      const wobble = ((Math.floor(frame / 4) + i) % 3 - 1) * 2;
      rect(W - 146 + i * 11 + wobble, down(14 + ((frame + i * 11) % fall)), 2, 2,
        mix(p.nav, p.fg, ink(78, 60)));
    }
  }
  if (sky.condition === "fog") {
    [H - 26, H - 18, H - 11].forEach((y, k) => {
      const x = down(W - 170 + ((k * 53 + frame * 0.5) % 200) - 20);
      rect(x, y, 54, 2, mix(p.nav, p.muted, 34));
      rect(x - 70, y + 2, 40, 2, mix(p.nav, p.muted, 24));
    });
  }
  if (sky.condition === "thunder" && frame % 53 >= 50) {
    rect(0, 0, W, H, mix(p.nav, p.fg, 12));
    rect(W - 84, 10, 4, 6, p.warning);
    rect(W - 88, 16, 6, 6, p.warning);
    rect(W - 86, 22, 4, 8, p.warning);
  }
}

/** The header's tint for the time of day, a few percent of an existing token. */
export function tint(p, phase) {
  const t = {
    night: [p.accent, ink(p, 9, 13)],
    dawn: [p.warning, ink(p, 8, 10)],
    dusk: [p.error, ink(p, 8, 9)],
  }[phase];
  return t ? mix(p.nav, t[0], t[1]) : null;
}
const ink = (p, dark, light) => (p.light ? light : dark);

function start() {
  const header = document.querySelector("header");
  if (!header || !globalThis.CanvasRenderingContext2D) return;
  const canvas = document.createElement("canvas");
  canvas.id = "sky";
  canvas.width = W;
  canvas.height = H;
  canvas.setAttribute("aria-hidden", "true");
  header.append(canvas);
  header.dataset.sky = "on";
  const ctx = canvas.getContext("2d");
  const still = matchMedia("(prefers-reduced-motion: reduce)");
  const reduced = () =>
    still.matches || document.documentElement.dataset.phoneReducedMotion === "true";
  let condition = null;
  let frame = 0;
  let timer = 0;
  let fetchedAt = 0;

  const draw = () => {
    const p = palette();
    const sky = skyAt(new Date(), condition);
    paint(ctx, p, sky, reduced() ? 0 : frame);
    const t = tint(p, sky.phase);
    if (t === null) header.style.removeProperty("--sky-nav");
    else header.style.setProperty("--sky-nav", css(t));
    canvas.title = condition
      ? `${LABELS[condition] || "Weather"}. Weather data from MET Norway`
      : "";
    return !reduced() && animated(sky.condition);
  };
  const loop = () => {
    clearTimeout(timer);
    if (document.hidden) return;
    const moving = draw();
    const focused = typeof document.hasFocus !== "function" || document.hasFocus();
    if (moving && focused) frame = (frame + 1) % 1000000;
    timer = setTimeout(loop, moving && focused ? 400 : 60000);
  };
  const weather = async () => {
    if (document.hidden || Date.now() - fetchedAt < WEATHER_EVERY) return;
    fetchedAt = Date.now();
    try {
      const response = await fetch("/api/weather", { method: "POST" });
      const body = response.ok ? await response.json() : null;
      condition = body && LABELS[body.condition] ? body.condition : null;
    } catch {
      condition = null;
    }
    loop();
  };
  document.addEventListener("visibilitychange", () => {
    loop();
    weather();
  });
  globalThis.addEventListener?.("focus", loop);
  globalThis.addEventListener?.("blur", loop);
  new MutationObserver(loop).observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["data-phone-theme", "data-phone-reduced-motion"],
  });
  setInterval(weather, WEATHER_EVERY + 1000);
  loop();
  weather();
}

if (typeof document !== "undefined" && document.readyState !== "loading") start();
else if (typeof document !== "undefined") document.addEventListener("DOMContentLoaded", start);
