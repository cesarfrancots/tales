/* Tales landing - a minimal terminal session with a few real interactions. */
(function () {
  'use strict';

  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const DEFAULT_STATUS = 'cache hit · debate complete · waiting for approval';
  const mobileQuery = window.matchMedia('(max-width: 640px)');
  let statusBase = DEFAULT_STATUS;
  let completeHeroIntro = null;

  const SCRIPT = [
    { kind: 'prompt', text: 'tales "make the right change"' },
    { kind: 'line', cls: 'tl-cc', text: 'Claude Code -> small scoped plan; no rewrite.' },
    { kind: 'line', cls: 'tl-cx', text: 'Codex -> test the edge before edits.' },
    { kind: 'rec', text: 'recommend Claude Code · confidence 0.82' },
    { kind: 'sys', text: 'gate locked · type /approve, /vote, or /hold' },
  ];
  const MOBILE_SCRIPT = [
    { kind: 'prompt', text: 'tales --workspace ~/project' },
    { kind: 'sys', text: 'context cache hit · local changes indexed' },
    { kind: 'prompt', text: '/ask "fix lint errors"' },
    { kind: 'line', cls: 'tl-cc', text: 'claude -> plan' },
    { kind: 'line', cls: 'tl-cx', text: 'codex -> check edge' },
    { kind: 'rec', text: 'recommend claude' },
    { kind: 'prompt', text: '/approve claude' },
    { kind: 'sys', text: 'gate released by you' },
  ];
  const activeScript = () => mobileQuery.matches ? MOBILE_SCRIPT : SCRIPT;

  function termParts() {
    return {
      body: document.getElementById('term-body'),
      cursor: document.getElementById('cursor'),
    };
  }

  function span(cls, text) {
    const node = document.createElement('span');
    if (cls) node.className = cls;
    node.textContent = text;
    return node;
  }

  function newLine(cls) {
    const { body, cursor } = termParts();
    if (!body || !cursor) return null;
    const node = document.createElement('div');
    node.className = 'tl' + (cls ? ' ' + cls : '');
    body.insertBefore(node, cursor);
    body.scrollTop = body.scrollHeight;
    return node;
  }

  async function typeInto(node, text, speed, token, getToken) {
    for (const ch of text) {
      if (token !== getToken()) return false;
      node.appendChild(document.createTextNode(ch));
      node.parentElement.scrollTop = node.parentElement.scrollHeight;
      await sleep(speed);
    }
    return true;
  }

  function renderScriptStatic(showCursor) {
    const { body, cursor } = termParts();
    if (!body || !cursor) return;
    body.querySelectorAll('.tl').forEach((node) => node.remove());
    for (const step of activeScript()) renderStep(step, true);
    cursor.style.display = showCursor ? '' : 'none';
  }

  function renderStep(step, staticMode) {
    if (step.kind === 'prompt') {
      const line = newLine('tl-prompt');
      if (!line) return null;
      line.appendChild(span('pc', '❯ '));
      line.appendChild(document.createTextNode(staticMode ? step.text : ''));
      return line;
    }
    if (step.kind === 'head') {
      const line = newLine(step.cls);
      if (!line) return null;
      line.appendChild(span('lbl', '▌ ' + step.name));
      line.appendChild(span('role', step.role));
      return line;
    }
    if (step.kind === 'line') {
      const line = newLine(step.cls + ' indent');
      if (line && staticMode) line.textContent = step.text;
      return line;
    }
    if (step.kind === 'rec') {
      const line = newLine('tl-rec');
      if (line && staticMode) line.textContent = '★ ' + step.text;
      return line;
    }
    const line = newLine('tl-sys');
    if (line) line.textContent = step.text;
    return line;
  }

  function initHero() {
    const { body, cursor } = termParts();
    if (!body || !cursor) return;
    let run = 0;
    let complete = false;

    async function play() {
      const token = ++run;
      complete = false;
      cursor.style.display = '';
      body.querySelectorAll('.tl').forEach((node) => node.remove());
      setGate('locked');
      setStatusReadout(DEFAULT_STATUS, true);

      for (const step of activeScript()) {
        if (token !== run) return;
        const line = renderStep(step, false);
        if (!line) continue;
        if (step.kind === 'prompt') {
          if (!(await typeInto(line, step.text, 13, token, () => run))) return;
        } else if (step.kind === 'line') {
          if (!(await typeInto(line, step.text, 5, token, () => run))) return;
        } else if (step.kind === 'rec') {
          if (!(await typeInto(line, '★ ' + step.text, 7, token, () => run))) return;
        } else {
          await sleep(85);
        }
        await sleep(55);
      }
      complete = true;
      if (mobileQuery.matches && token === run) {
        await sleep(1300);
        if (token === run && mobileQuery.matches) play();
      }
    }

    completeHeroIntro = () => {
      if (!complete) {
        run++;
        renderScriptStatic(true);
        complete = true;
      }
    };

    if (reduced) {
      renderScriptStatic(false);
      complete = true;
    } else {
      let started = false;
      const io = new IntersectionObserver((entries) => {
        if (entries[0].isIntersecting && !started) {
          started = true;
          play();
          io.disconnect();
        }
      }, { threshold: 0.25 });
      io.observe(body);
    }

    const replay = document.getElementById('replay');
    if (replay) replay.addEventListener('click', () => play());
    mobileQuery.addEventListener('change', () => {
      if (complete) renderScriptStatic(true);
    });
  }

  function setGate(state) {
    const tag = document.getElementById('gate-state');
    if (!tag) return;
    tag.classList.remove('released', 'held');
    if (state === 'released') {
      tag.textContent = 'gate released';
      tag.classList.add('released');
    } else if (state === 'held') {
      tag.textContent = 'plan held';
      tag.classList.add('held');
    } else {
      tag.textContent = 'gate locked';
    }
  }

  function setStatusReadout(text, persist) {
    const readout = document.getElementById('status-readout');
    const target = readout && readout.querySelector('span');
    const value = text || statusBase || DEFAULT_STATUS;
    if (persist) statusBase = value;
    if (target) target.textContent = value;
  }

  function appendTerminalLine(cls, text, parts) {
    const { body, cursor } = termParts();
    if (!body || !cursor) return null;
    if (cursor.style.display === 'none') cursor.style.display = '';
    const line = document.createElement('div');
    line.className = 'tl tl-runtime' + (cls ? ' ' + cls : '');
    if (parts) {
      for (const part of parts) line.appendChild(span(part.cls || '', part.text || ''));
    } else {
      line.textContent = text || '';
    }
    body.insertBefore(line, cursor);
    body.scrollTop = body.scrollHeight;
    return line;
  }

  function appendCommandLine(command) {
    return appendTerminalLine('tl-prompt', '', [
      { cls: 'pc', text: '❯ ' },
      { text: command },
    ]);
  }

  function clearRuntimeLines() {
    document.querySelectorAll('.tl-runtime').forEach((node) => node.remove());
  }

  function normalizeCommand(raw) {
    let command = (raw || '').trim().toLowerCase();
    if (!command) return '';
    if (!command.startsWith('/')) command = '/' + command;
    command = command.replace(/\s+/g, ' ');
    if (command === '/confirm' || command === '/confirm 1' || command === '/approve claude') {
      return '/approve';
    }
    if (command === '/reject' || command === '/stop') return '/hold';
    return command;
  }

  function runCommand(raw) {
    const command = normalizeCommand(raw);
    if (!command) return;
    if (completeHeroIntro) completeHeroIntro();

    if (command === '/replay') {
      const replay = document.getElementById('replay');
      replay && replay.click();
      return;
    }

    clearRuntimeLines();
    appendCommandLine(command);

    if (command === '/context') {
      appendTerminalLine('tl-sys indent', 'project map cache reused · prompt forecast stays lean');
      appendTerminalLine('tl-sys indent', 'known payload: about 19% of the planning budget');
      setGate('locked');
      setStatusReadout('context ready · cache reused · no executor started', true);
      return;
    }

    if (command === '/vote') {
      appendTerminalLine('tl-rec', '★ recommendation: Claude Code · confidence 0.82');
      appendTerminalLine('tl-cx indent', 'Codex: approve after callback tests are listed');
      setGate('locked');
      setStatusReadout('vote refreshed · recommendation remains advisory', true);
      return;
    }

    if (command === '/approve') {
      appendTerminalLine('tl-ok', '✓ gate released · Claude Code executing');
      appendTerminalLine('tl-sys indent', 'isolated worktree · local changes preserved');
      appendTerminalLine('tl-diff indent', '', [
        { cls: 'meta', text: '+ ' },
        { cls: 'file', text: 'src/auth/google.ts' },
        { cls: 'meta', text: ' · callback wiring' },
      ]);
      appendTerminalLine('tl-ok', '✓ diff ready · waiting for your review');
      setGate('released');
      setStatusReadout('released · executor running · diff ready', true);
      return;
    }

    if (command === '/hold') {
      appendTerminalLine('tl-warn', '▣ plan held · no executor started');
      appendTerminalLine('tl-sys indent', 'resume packet saved for the next Tales run');
      setGate('held');
      setStatusReadout('held · executor skipped · resume packet ready', true);
      return;
    }

    appendTerminalLine('tl-warn', 'unknown command · gate remains locked');
    setGate('locked');
    setStatusReadout('unknown command · try /vote, /approve, or /hold', true);
  }

  function initCommandBar() {
    const strip = document.getElementById('command-strip');
    const bufferBox = document.getElementById('command-buffer');
    const bufferText = document.getElementById('command-buffer-text');
    const buttons = Array.from(document.querySelectorAll('.cmd-chip[data-command]'));
    if (!strip || !bufferBox || !bufferText || !buttons.length) return;
    let commandMode = false;
    let buffer = '/';

    const setCommandMode = (active) => {
      commandMode = active;
      strip.classList.toggle('commanding', active);
      if (!active) buffer = '/';
      bufferText.textContent = buffer;
      bufferBox.setAttribute('aria-hidden', String(!active));
    };

    buttons.forEach((button) => {
      button.addEventListener('click', () => {
        buttons.forEach((candidate) => candidate.classList.toggle('active', candidate === button));
        window.setTimeout(() => button.classList.remove('active'), 360);
        runCommand(button.dataset.command);
      });
    });

    document.addEventListener('keydown', (event) => {
      const target = event.target;
      const tagName = target && target.tagName;
      const isEditable = target && (
        target.isContentEditable ||
        tagName === 'INPUT' ||
        tagName === 'SELECT' ||
        tagName === 'TEXTAREA'
      );
      if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey || isEditable) return;

      if (!commandMode) {
        if (event.key === '/') {
          event.preventDefault();
          if (completeHeroIntro) completeHeroIntro();
          setCommandMode(true);
        } else if (event.key === 'Enter') {
          event.preventDefault();
          runCommand('/approve');
        }
        return;
      }

      event.preventDefault();
      if (event.key === 'Escape') {
        setCommandMode(false);
        setStatusReadout(statusBase);
      } else if (event.key === 'Enter') {
        const command = buffer;
        setCommandMode(false);
        runCommand(command);
      } else if (event.key === 'Backspace') {
        buffer = buffer.length > 1 ? buffer.slice(0, -1) : '/';
        bufferText.textContent = buffer;
      } else if (event.key.length === 1 && /^[a-zA-Z0-9/ _-]$/.test(event.key)) {
        if (buffer.length < 32) {
          const next = event.key === '/' && buffer.endsWith('/') ? '' : event.key;
          buffer += next;
          bufferText.textContent = buffer;
        }
      }
    });
  }

  function initReveal() {
    const els = document.querySelectorAll('.band .wrap > *, .line-card, .run-timeline > div, .report > div');
    els.forEach((el, i) => {
      el.classList.add('reveal');
      el.style.transitionDelay = (Math.min(i % 6, 5) * 0.04) + 's';
    });
    if (reduced) { els.forEach((el) => el.classList.add('in')); return; }
    const io = new IntersectionObserver((entries) => {
      entries.forEach((entry) => {
        if (entry.isIntersecting) {
          entry.target.classList.add('in');
          io.unobserve(entry.target);
        }
      });
    }, { threshold: 0.14 });
    els.forEach((el) => io.observe(el));
  }

  function initStats() {
    const nums = document.querySelectorAll('.stat-n[data-count]');
    const animate = (el) => {
      const target = parseFloat(el.dataset.count);
      const decimals = (el.dataset.count.split('.')[1] || '').length;
      if (reduced || target === 0) { el.textContent = target.toFixed(decimals); return; }
      const start = performance.now();
      const duration = 900;
      const tick = (now) => {
        const p = Math.min((now - start) / duration, 1);
        const eased = 1 - Math.pow(1 - p, 3);
        el.textContent = (target * eased).toFixed(decimals);
        if (p < 1) requestAnimationFrame(tick);
        else el.textContent = target.toFixed(decimals);
      };
      requestAnimationFrame(tick);
    };
    const io = new IntersectionObserver((entries) => {
      entries.forEach((entry) => {
        if (entry.isIntersecting) {
          animate(entry.target);
          io.unobserve(entry.target);
        }
      });
    }, { threshold: 0.5 });
    nums.forEach((num) => io.observe(num));
  }

  function initVideo() {
    const video = document.getElementById('demo-video');
    const btn = document.getElementById('demo-play');
    if (!video || !btn) return;
    let userPaused = false;
    const set = (playing) => {
      btn.dataset.playing = String(playing);
      btn.setAttribute('aria-label', playing ? 'Pause video' : 'Play video');
    };
    btn.addEventListener('click', () => {
      if (video.paused) {
        video.play().then(() => { userPaused = false; set(true); }).catch(() => {});
      } else {
        video.pause();
        userPaused = true;
        set(false);
      }
    });
    new IntersectionObserver((entries) => {
      if (entries[0].isIntersecting) {
        if (!userPaused) video.play().then(() => set(true)).catch(() => {});
      } else if (!video.paused) {
        video.pause();
        set(false);
      }
    }, { threshold: 0.4 }).observe(video);
  }

  document.addEventListener('DOMContentLoaded', () => {
    initHero();
    initCommandBar();
    initReveal();
    initStats();
    initVideo();
  });
}());
