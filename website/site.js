// os.beecoders.com: small touches on top of a page that is complete without them.
(() => {
  const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
  document.documentElement.classList.add("js");

  // Sections fade in as they scroll into view.
  const seen = new IntersectionObserver((entries) => {
    for (const e of entries) if (e.isIntersecting) { e.target.classList.add("in"); seen.unobserve(e.target); }
  }, { threshold: 0.12 });
  document.querySelectorAll("main > section:not(.hero), .card, .safe-grid > div, .timeline li, .layer")
    .forEach((el) => { el.classList.add("reveal"); seen.observe(el); });

  // Numbers count up once.
  const fmt = (n, end) => end >= 10000 ? (n / 1000).toFixed(1) + "k" : String(Math.round(n));
  const counter = new IntersectionObserver((entries) => {
    for (const e of entries) {
      if (!e.isIntersecting) continue;
      counter.unobserve(e.target);
      const end = +e.target.dataset.count, t0 = performance.now(), dur = 1400;
      const tick = (t) => {
        const k = Math.min(1, (t - t0) / dur), ease = 1 - Math.pow(1 - k, 3);
        e.target.textContent = fmt(end * ease, end);
        if (k < 1) requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    }
  });
  if (!still) document.querySelectorAll("[data-count]").forEach((el) => counter.observe(el));

  // The improvement loop: light up one step at a time.
  const nodes = document.querySelectorAll(".loop-svg .nodes g");
  const steps = document.querySelectorAll(".steps li");
  if (nodes.length && !still) {
    let i = 0;
    const show = () => {
      nodes.forEach((n, j) => n.classList.toggle("on", j === i));
      steps.forEach((s, j) => s.classList.toggle("on", j === i));
      i = (i + 1) % nodes.length;
    };
    show();
    setInterval(show, 2200);
  }

  // Cards: a soft glow that follows the mouse.
  document.querySelectorAll(".card").forEach((c) => c.addEventListener("pointermove", (ev) => {
    const r = c.getBoundingClientRect();
    c.style.setProperty("--mx", ev.clientX - r.left + "px");
    c.style.setProperty("--my", ev.clientY - r.top + "px");
  }));

  // The hero terminal replays its session: the command is typed, the rest appears line by line.
  const term = document.getElementById("term");
  if (!term || still) return;
  const lines = term.textContent.replace(/\n+$/, "").split("\n");
  const prompt = "bee:/data$ ";
  const kind = (l) =>
    l.startsWith("I will") || l.startsWith("and show") ? "m" :
    l.startsWith("Run them") ? "q" :
    l.startsWith("$ ") || l.trim() === "…" ? "d" :
    l.startsWith("done:") ? "ok" : "";
  const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  const play = async () => {
    for (;;) {
      term.innerHTML = "";
      for (const line of lines) {
        if (line.startsWith(prompt)) {
          const cmd = line.slice(prompt.length);
          const row = document.createElement("span");
          row.innerHTML = `<span class="p">${prompt}</span>`;
          const typed = document.createTextNode("");
          const cur = document.createElement("span");
          cur.className = "cursor";
          row.append(typed, cur);
          term.append(row);
          await sleep(700);
          for (const ch of cmd) { typed.data += ch; await sleep(18 + Math.random() * 40); }
          if (!cmd) { await sleep(6000); break; }
          cur.remove();
          term.append("\n");
          await sleep(900);
        } else {
          const k = kind(line);
          term.insertAdjacentHTML("beforeend", (k ? `<span class="${k}">${esc(line)}</span>` : esc(line)) + "\n");
          await sleep(k === "q" ? 1400 : 160);
        }
      }
    }
  };

  // Start when the terminal is visible.
  const go = new IntersectionObserver((e) => { if (e[0].isIntersecting) { go.disconnect(); play(); } });
  go.observe(term);
})();
