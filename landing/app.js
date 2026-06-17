/* Tales landing — the page narrates itself like a Tales session. */
(function () {
  'use strict';

  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  // ── The script Tales "types" in the hero ────────────────────────────────
  // kind: prompt | sys | tales | head | line | rec | pick | you | ok | gap
  const SCRIPT = [
    { kind: 'prompt', text: 'tales' },
    { kind: 'sys', text: '● two AI coders, one terminal — you stay on the trigger' },
    { kind: 'gap' },
    { kind: 'tales', text: 'hey. I put Claude Code and Codex in the same room and let them' },
    { kind: 'tales', text: 'argue about your task before a single line gets written.' },
    { kind: 'tales', text: 'one drafts a plan. the other tears into it. they reach a verdict —' },
    { kind: 'tales', text: 'then you choose who runs it. watch:' },
    { kind: 'gap' },
    { kind: 'prompt', text: 'tales "add OAuth login"' },
    { kind: 'gap' },
    { kind: 'head', cls: 'tl-cc', name: 'Claude Code', role: 'DRAFTER' },
    { kind: 'line', cls: 'tl-cc', text: 'middleware layer — passport.js + the Google strategy, wired into' },
    { kind: 'line', cls: 'tl-cc', text: 'the existing session store. small, isolated change.' },
    { kind: 'gap' },
    { kind: 'head', cls: 'tl-cx', name: 'Codex', role: 'CRITIC' },
    { kind: 'line', cls: 'tl-cx', text: 'approach is right. Claude has the better read on the current auth' },
    { kind: 'line', cls: 'tl-cx', text: "flow — let it execute. I'll review the diff after." },
    { kind: 'gap' },
    { kind: 'rec', text: 'recommend  Claude Code' },
    { kind: 'pick' },
    { kind: 'gap' },
    { kind: 'you', text: 'You ▸ 1' },
    { kind: 'ok', text: '✓ Claude Code executing in an isolated git worktree…' },
    { kind: 'ok', text: '✓ done — clean diff ready for your review.' },
    { kind: 'gap' },
    { kind: 'tales', text: 'that’s it. two minds, your call, nothing runs behind your back.' },
  ];

  // ── Hero typewriter ─────────────────────────────────────────────────────
  function initHero() {
    const body = document.getElementById('term-body');
    const cursor = document.getElementById('cursor');
    if (!body || !cursor) return;
    let run = 0; // run token — bump to cancel an in-flight run

    const span = (cls, text) => {
      const s = document.createElement('span');
      if (cls) s.className = cls;
      s.textContent = text;
      return s;
    };
    const newLine = (cls) => {
      const d = document.createElement('div');
      d.className = 'tl' + (cls ? ' ' + cls : '');
      body.insertBefore(d, cursor);
      return d;
    };

    async function type(el, text, speed, token) {
      for (const ch of text) {
        if (token !== run) return false;
        el.appendChild(document.createTextNode(ch));
        await sleep(speed);
      }
      return true;
    }

    async function play() {
      const token = ++run;
      body.querySelectorAll('.tl').forEach((n) => n.remove());

      for (const step of SCRIPT) {
        if (token !== run) return;
        switch (step.kind) {
          case 'gap':
            newLine().innerHTML = '&nbsp;';
            break;
          case 'prompt': {
            const l = newLine('tl-prompt');
            l.appendChild(span('pc', '❯ '));
            if (!(await type(l, step.text, 34, token))) return;
            break;
          }
          case 'sys':
            newLine('tl-sys').textContent = step.text;
            await sleep(reduced ? 0 : 260);
            break;
          case 'tales':
            if (!(await type(newLine('tl-tales'), step.text, 22, token))) return;
            break;
          case 'head': {
            const l = newLine(step.cls);
            l.appendChild(span('lbl', '▌ ' + step.name));
            l.appendChild(span('role', step.role));
            await sleep(reduced ? 0 : 160);
            break;
          }
          case 'line': {
            const l = newLine(step.cls + ' indent');
            if (!(await type(l, step.text, 12, token))) return;
            break;
          }
          case 'rec': {
            const l = newLine('tl-rec');
            l.appendChild(span(null, '★ '));
            await type(l, step.text, 16, token);
            break;
          }
          case 'pick': {
            const l = newLine('tl-rec indent');
            l.appendChild(span(null, '▸ pick executor   '));
            l.appendChild(span('tl-cc', '[1] Claude Code'));
            l.appendChild(span('muted', '   '));
            l.appendChild(span('tl-cx', '[2] Codex'));
            l.appendChild(span('tl-you', '      ← you decide'));
            await sleep(reduced ? 0 : 520);
            break;
          }
          case 'you':
            if (!(await type(newLine('tl-you'), step.text, 60, token))) return;
            await sleep(reduced ? 0 : 260);
            break;
          case 'ok':
            newLine('tl-ok').textContent = step.text;
            await sleep(reduced ? 0 : 420);
            break;
        }
        await sleep(reduced ? 0 : 90);
      }
    }

    if (reduced) {
      // Render everything at once, no animation.
      run++;
      body.querySelectorAll('.tl').forEach((n) => n.remove());
      for (const step of SCRIPT) {
        if (step.kind === 'gap') { newLine().innerHTML = '&nbsp;'; continue; }
        const cls = { prompt: 'tl-prompt', sys: 'tl-sys', tales: 'tl-tales', rec: 'tl-rec', you: 'tl-you', ok: 'tl-ok', line: (step.cls||'') + ' indent', head: step.cls }[step.kind] || '';
        const l = newLine(cls);
        if (step.kind === 'head') { l.appendChild(span('lbl', '▌ ' + step.name)); l.appendChild(span('role', step.role)); }
        else if (step.kind === 'prompt') { l.appendChild(span('pc', '❯ ')); l.appendChild(document.createTextNode(step.text)); }
        else if (step.kind === 'rec') l.textContent = '★ ' + step.text;
        else if (step.kind === 'pick') l.textContent = '▸ pick executor   [1] Claude Code   [2] Codex      ← you decide';
        else l.textContent = step.text || '';
      }
      cursor.style.display = 'none';
      return;
    }

    // Start when the hero is on screen; offer manual replay.
    let started = false;
    const io = new IntersectionObserver((es) => {
      if (es[0].isIntersecting && !started) { started = true; play(); io.disconnect(); }
    }, { threshold: 0.25 });
    io.observe(body);

    const replay = document.getElementById('replay');
    if (replay) replay.addEventListener('click', () => play());
  }

  // ── Scroll reveal ───────────────────────────────────────────────────────
  function initReveal() {
    const els = document.querySelectorAll('.band .wrap > *, .stat, .why-card, .run-step');
    els.forEach((el, i) => {
      el.classList.add('reveal');
      el.style.transitionDelay = (Math.min(i % 6, 5) * 0.05) + 's';
    });
    if (reduced) { els.forEach((el) => el.classList.add('in')); return; }
    const io = new IntersectionObserver((entries) => {
      entries.forEach((e) => { if (e.isIntersecting) { e.target.classList.add('in'); io.unobserve(e.target); } });
    }, { threshold: 0.14 });
    els.forEach((el) => io.observe(el));
  }

  // ── Stat count-up ───────────────────────────────────────────────────────
  function initStats() {
    const nums = document.querySelectorAll('.stat-n[data-count]');
    const animate = (el) => {
      const target = parseFloat(el.dataset.count);
      const decimals = (el.dataset.count.split('.')[1] || '').length;
      if (reduced || target === 0) { el.textContent = target.toFixed(decimals); return; }
      const start = performance.now(), dur = 900;
      const tick = (now) => {
        const p = Math.min((now - start) / dur, 1);
        const eased = 1 - Math.pow(1 - p, 3);
        el.textContent = (target * eased).toFixed(decimals);
        if (p < 1) requestAnimationFrame(tick);
        else el.textContent = target.toFixed(decimals);
      };
      requestAnimationFrame(tick);
    };
    const io = new IntersectionObserver((entries) => {
      entries.forEach((e) => { if (e.isIntersecting) { animate(e.target); io.unobserve(e.target); } });
    }, { threshold: 0.5 });
    nums.forEach((n) => io.observe(n));
  }

  // ── Demo video ──────────────────────────────────────────────────────────
  function initVideo() {
    const video = document.getElementById('demo-video');
    const btn = document.getElementById('demo-play');
    if (!video || !btn) return;
    let userPaused = false;
    const set = (p) => { btn.dataset.playing = String(p); btn.setAttribute('aria-label', p ? 'Pause' : 'Play'); };
    btn.addEventListener('click', () => {
      if (video.paused) video.play().then(() => { userPaused = false; set(true); }).catch(() => {});
      else { video.pause(); userPaused = true; set(false); }
    });
    new IntersectionObserver((es) => {
      if (es[0].isIntersecting) { if (!userPaused) video.play().then(() => set(true)).catch(() => {}); }
      else if (!video.paused) { video.pause(); set(false); }
    }, { threshold: 0.4 }).observe(video);
  }

  document.addEventListener('DOMContentLoaded', () => {
    initHero();
    initReveal();
    initStats();
    initVideo();
  });
}());
