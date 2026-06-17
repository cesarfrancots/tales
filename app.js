(function () {
  'use strict';

  const prefersReduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  // Gate CSS hidden states on JS availability — prevents invisible content on JS failure
  document.documentElement.classList.add('js-loaded');

  function delay(ms) { return new Promise(r => setTimeout(r, ms)); }

  // Split HTML string into text segments and tag segments for safe char-level typing
  function parseHTML(html) {
    const parts = [];
    const re = /<[^>]+>/g;
    let last = 0, m;
    while ((m = re.exec(html)) !== null) {
      if (m.index > last) parts.push({ tag: false, s: html.slice(last, m.index) });
      parts.push({ tag: true, s: m[0] });
      last = m.index + m[0].length;
    }
    if (last < html.length) parts.push({ tag: false, s: html.slice(last) });
    return parts;
  }

  // ── Hero entrance ─────────────────────────────────────────────
  function initHero() {
    const hero = document.getElementById('hero');
    if (!hero) return;
    // Double rAF: first ensures style recalc has seen the hidden state, second triggers transition
    requestAnimationFrame(() => requestAnimationFrame(() => hero.classList.add('hero-entered')));
  }

  // ── Scroll reveal ─────────────────────────────────────────────
  function initReveal() {
    const els = document.querySelectorAll('.reveal, .reveal-slide');
    if (!els.length) return;

    if (prefersReduced) {
      els.forEach(el => el.classList.add('is-visible'));
      return;
    }

    // Set CSS custom property for stagger delay before observing
    els.forEach(el => {
      if (el.dataset.delay) el.style.setProperty('--delay', el.dataset.delay);
    });

    const obs = new IntersectionObserver(entries => {
      entries.forEach(e => {
        if (!e.isIntersecting) return;
        e.target.classList.add('is-visible');
        obs.unobserve(e.target);
      });
    }, { threshold: 0.12 });

    els.forEach(el => obs.observe(el));
  }

  // ── Terminal chat typing ──────────────────────────────────────
  function initTerminal() {
    if (prefersReduced) return;

    const body = document.querySelector('.terminal-body');
    if (!body) return;

    const msgs = Array.from(body.querySelectorAll('.chat-msg'));
    if (!msgs.length) return;

    const data = msgs.map(m => {
      const textEl = m.querySelector('.chat-text');
      return {
        el: m,
        textEl,
        textHTML: textEl ? textEl.innerHTML : '',
        badgeEl: m.querySelector('.approval-badge'),
      };
    });

    // Hide all messages; JS will replay them
    data.forEach(({ el, textEl, badgeEl }) => {
      el.style.opacity = '0';
      el.style.transition = 'opacity .3s ease';
      if (textEl) textEl.innerHTML = '';
      if (badgeEl) {
        badgeEl.style.opacity = '0';
        badgeEl.style.transition = 'opacity .35s ease';
      }
    });

    let visible = false;
    let looping = false;

    new IntersectionObserver(entries => {
      visible = entries[0].isIntersecting;
      if (visible && !looping) kickoff();
    }, { threshold: 0.25 }).observe(body);

    async function typeEl(el, html) {
      const segs = parseHTML(html);
      let built = '';
      for (const seg of segs) {
        if (seg.tag) {
          // Inject HTML tags instantly to preserve styling (e.g. <code>)
          built += seg.s;
          el.innerHTML = built;
        } else {
          for (const ch of seg.s) {
            if (!visible) return false;
            built += ch;
            el.innerHTML = built;
            await delay(15);
          }
        }
      }
      return true;
    }

    async function run() {
      looping = true;
      for (const d of data) {
        if (!visible) { looping = false; return; }
        d.el.style.opacity = '1';
        if (d.textEl) d.textEl.innerHTML = '';
        if (d.badgeEl) d.badgeEl.style.opacity = '0';
        await delay(200);
        if (d.textEl) {
          const ok = await typeEl(d.textEl, d.textHTML);
          if (!ok) { looping = false; return; }
        }
        if (d.badgeEl) { await delay(150); d.badgeEl.style.opacity = '1'; }
        await delay(700);
      }

      await delay(3500);

      // Fade out all messages, then clear for next loop
      data.forEach(d => { d.el.style.opacity = '0'; });
      await delay(400);
      data.forEach(({ textEl, badgeEl }) => {
        if (textEl) textEl.innerHTML = '';
        if (badgeEl) badgeEl.style.opacity = '0';
      });
      await delay(350);

      if (visible) run();
      else looping = false;
    }

    function kickoff() {
      looping = false;
      setTimeout(run, 900);
    }

    kickoff();
  }

  // ── Demo video ────────────────────────────────────────────────
  function initVideo() {
    const video = document.getElementById('demo-video');
    const btn = document.getElementById('demo-play-btn');
    if (!video || !btn) return;

    let userPaused = false;

    function setState(playing) {
      btn.dataset.playing = String(playing);
      btn.setAttribute('aria-label', playing ? 'Pause video' : 'Play video');
    }

    btn.addEventListener('click', () => {
      if (video.paused) {
        video.play().then(() => { userPaused = false; setState(true); }).catch(() => {});
      } else {
        video.pause();
        userPaused = true;
        setState(false);
      }
    });

    new IntersectionObserver(entries => {
      if (entries[0].isIntersecting) {
        if (!userPaused) video.play().then(() => setState(true)).catch(() => {});
      } else {
        if (!video.paused) { video.pause(); setState(false); }
      }
    }, { threshold: 0.4 }).observe(video);
  }

  // ── Button press micro-interaction ────────────────────────────
  function initPress() {
    document.querySelectorAll('.btn').forEach(el => {
      el.addEventListener('pointerdown', () => el.classList.add('is-pressing'));
      ['pointerup', 'pointerleave', 'pointercancel'].forEach(ev =>
        el.addEventListener(ev, () => el.classList.remove('is-pressing')));
    });
  }

  // ── Boot ──────────────────────────────────────────────────────
  document.addEventListener('DOMContentLoaded', () => {
    initHero();
    initReveal();
    initTerminal();
    initVideo();
    initPress();
  });
}());
