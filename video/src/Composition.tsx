import React from "react";
import {
  AbsoluteFill,
  Sequence,
  useCurrentFrame,
  useVideoConfig,
  interpolate,
  Easing,
  spring,
} from "remotion";
import { C, mono, sans } from "./theme";

const EASE = Easing.bezier(0.16, 1, 0.3, 1);

const reveal = (frame: number, delay: number, dur = 18) => {
  const p = interpolate(frame - delay, [0, dur], [0, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
    easing: EASE,
  });
  return { opacity: p, y: (1 - p) * 22 };
};

const Caret: React.FC<{ color?: string }> = ({ color = C.accent }) => {
  const frame = useCurrentFrame();
  const on = Math.floor(frame / 15) % 2 === 0;
  return <span style={{ color, opacity: on ? 1 : 0.18 }}>▌</span>;
};

// Per-scene fade in/out for smooth cuts.
const Fade: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const frame = useCurrentFrame();
  const { durationInFrames } = useVideoConfig();
  const opacity = interpolate(
    frame,
    [0, 10, durationInFrames - 12, durationInFrames],
    [0, 1, 1, 0],
    { extrapolateLeft: "clamp", extrapolateRight: "clamp" },
  );
  return <AbsoluteFill style={{ opacity }}>{children}</AbsoluteFill>;
};

const Bg: React.FC = () => (
  <AbsoluteFill
    style={{
      background: `radial-gradient(1400px 800px at 50% -15%, ${C.bg2}, ${C.bg})`,
    }}
  >
    <AbsoluteFill
      style={{
        backgroundImage: `linear-gradient(${C.border}12 1px, transparent 1px), linear-gradient(90deg, ${C.border}12 1px, transparent 1px)`,
        backgroundSize: "52px 52px",
        opacity: 0.5,
        maskImage: "radial-gradient(900px 600px at 50% 40%, black, transparent)",
        WebkitMaskImage: "radial-gradient(900px 600px at 50% 40%, black, transparent)",
      }}
    />
  </AbsoluteFill>
);

const Block: React.FC<{
  color: string;
  name: string;
  role?: string;
  delay: number;
  cost?: string;
  children: React.ReactNode;
}> = ({ color, name, role, delay, cost, children }) => {
  const frame = useCurrentFrame();
  const { opacity, y } = reveal(frame, delay, 18);
  return (
    <div
      style={{
        opacity,
        transform: `translateY(${y}px)`,
        width: 1180,
        background: C.panel,
        border: `1px solid ${C.border}`,
        borderLeft: `3px solid ${color}`,
        borderRadius: 14,
        padding: "22px 28px",
        marginBottom: 22,
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 14, marginBottom: 12 }}>
        <div style={{ width: 11, height: 11, borderRadius: 11, background: color }} />
        <div style={{ color, fontFamily: mono, fontWeight: 700, fontSize: 25 }}>{name}</div>
        {role && (
          <div
            style={{
              color: C.faint,
              fontFamily: mono,
              fontSize: 14,
              letterSpacing: 1.6,
              border: `1px solid ${C.border}`,
              borderRadius: 6,
              padding: "2px 9px",
            }}
          >
            {role}
          </div>
        )}
        {cost && (
          <div style={{ marginLeft: "auto", color: C.faint, fontFamily: mono, fontSize: 16 }}>
            {cost}
          </div>
        )}
      </div>
      <div style={{ color: C.text, fontFamily: mono, fontSize: 22, lineHeight: 1.65 }}>
        {children}
      </div>
    </div>
  );
};

const Intro: React.FC = () => {
  const frame = useCurrentFrame();
  const a = reveal(frame, 6, 22);
  const b = reveal(frame, 30, 24);
  const scale = interpolate(frame, [0, 30], [0.96, 1], {
    extrapolateRight: "clamp",
    easing: EASE,
  });
  return (
    <AbsoluteFill style={{ justifyContent: "center", alignItems: "center" }}>
      <div style={{ transform: `scale(${scale})`, textAlign: "center" }}>
        <div
          style={{
            opacity: a.opacity,
            transform: `translateY(${a.y}px)`,
            fontFamily: mono,
            fontSize: 100,
            fontWeight: 800,
            letterSpacing: 2,
          }}
        >
          <span style={{ color: C.accent }}>❯</span> <span style={{ color: C.text }}>tales</span>
          <Caret />
        </div>
        <div
          style={{
            opacity: b.opacity,
            transform: `translateY(${b.y}px)`,
            marginTop: 30,
            fontFamily: sans,
            fontSize: 38,
            color: C.dim,
          }}
        >
          Two AIs plan. <span style={{ color: C.you }}>You decide</span> what runs.
        </div>
      </div>
    </AbsoluteFill>
  );
};

const Collab: React.FC = () => {
  const frame = useCurrentFrame();
  const head = reveal(frame, 0, 18);
  return (
    <AbsoluteFill style={{ padding: "80px 130px" }}>
      <div
        style={{
          opacity: head.opacity,
          transform: `translateY(${head.y}px)`,
          display: "flex",
          alignItems: "center",
          gap: 16,
          marginBottom: 34,
        }}
      >
        <span style={{ color: C.accent, fontFamily: mono, fontSize: 28, fontWeight: 700 }}>
          ❯ tales
        </span>
        <span style={{ color: C.dim, fontFamily: mono, fontSize: 22 }}>· refine the landing page</span>
        <span
          style={{
            marginLeft: "auto",
            color: C.accent,
            fontFamily: mono,
            fontSize: 18,
            border: `1px solid ${C.border}`,
            borderRadius: 999,
            padding: "4px 14px",
          }}
        >
          planning
        </span>
      </div>
      <Block color={C.claude} name="Claude Code" role="DRAFTER" delay={26} cost="$0.14">
        Draft — add OG + Twitter tags, tighten the hero, a quickstart{" "}
        (<span style={{ color: C.accent }}>cargo build · tales-tui</span>), and a media + skills
        line. Surgical edits, no new sections.
      </Block>
      <Block color={C.codex} name="Codex" role="CRITIC" delay={150}>
        The draft matches the scope — but a couple details are under-specified. Checking the CSS so
        the quickstart and responsive behavior hold up.
      </Block>
    </AbsoluteFill>
  );
};

const Gate: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const rec = reveal(frame, 0, 18);
  const appr = spring({ frame: frame - 42, fps, config: { damping: 13 } });
  const note = reveal(frame, 64, 20);
  return (
    <AbsoluteFill style={{ justifyContent: "center", alignItems: "center" }}>
      <div
        style={{
          opacity: rec.opacity,
          transform: `translateY(${rec.y}px)`,
          width: 780,
          background: "#0d1817",
          border: "1px solid #1f3b38",
          borderRadius: 16,
          padding: "26px 32px",
          marginBottom: 34,
        }}
      >
        <div style={{ color: C.accent, fontFamily: mono, fontSize: 28, fontWeight: 700 }}>
          ★ recommend Claude Code
        </div>
        <div style={{ color: C.dim, fontFamily: mono, fontSize: 20, marginTop: 8 }}>
          both agents agree · confidence 0.9
        </div>
      </div>
      <div
        style={{
          transform: `scale(${interpolate(appr, [0, 1], [0.8, 1])})`,
          opacity: appr,
          background: "#11351f",
          border: "1px solid #1f6f3f",
          color: C.you,
          fontFamily: mono,
          fontSize: 26,
          fontWeight: 700,
          borderRadius: 11,
          padding: "14px 30px",
        }}
      >
        ✓ approve &amp; run
      </div>
      <div
        style={{
          opacity: note.opacity,
          transform: `translateY(${note.y}px)`,
          marginTop: 38,
          fontFamily: sans,
          fontSize: 30,
          color: C.dim,
        }}
      >
        nothing runs until <span style={{ color: C.you }}>you</span> approve.
      </div>
    </AbsoluteFill>
  );
};

const Execute: React.FC = () => {
  const frame = useCurrentFrame();
  const execFade = interpolate(frame, [0, 14, 48, 64], [0, 1, 1, 0], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
  });
  const page = reveal(frame, 40, 26);
  const pageScale = interpolate(frame - 40, [0, 30], [0.94, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
    easing: EASE,
  });
  return (
    <AbsoluteFill style={{ justifyContent: "center", alignItems: "center" }}>
      <div
        style={{
          opacity: execFade,
          position: "absolute",
          top: 200,
          color: C.claude,
          fontFamily: mono,
          fontSize: 28,
        }}
      >
        ▌ Claude Code executing…
      </div>
      <div
        style={{
          opacity: page.opacity,
          transform: `translateY(${page.y}px) scale(${pageScale})`,
          width: 1280,
          background: "#0a0c0f",
          border: `1px solid ${C.border}`,
          borderRadius: 18,
          padding: 64,
          boxShadow: "0 40px 120px rgba(0,0,0,0.55)",
        }}
      >
        <div style={{ color: C.dim, fontFamily: mono, fontSize: 18, marginBottom: 20 }}>
          ● OPEN SOURCE · MIT
        </div>
        <div style={{ fontFamily: sans, fontSize: 74, fontWeight: 800, color: C.text, lineHeight: 1.05 }}>
          Two AIs plan.
          <br />
          <span style={{ color: C.accent }}>You decide</span> what runs.
        </div>
        <div style={{ fontFamily: sans, fontSize: 26, color: C.dim, marginTop: 26, maxWidth: 820 }}>
          Tales puts Claude Code and Codex in a shared chat on your task — they debate, draft a
          plan, and nominate who executes.
        </div>
        <div
          style={{
            marginTop: 32,
            fontFamily: mono,
            fontSize: 21,
            color: C.you,
            background: "#0d1610",
            border: `1px solid ${C.border}`,
            borderRadius: 10,
            padding: "14px 20px",
            width: "fit-content",
          }}
        >
          cargo build --release && tales-tui "add OAuth"
        </div>
      </div>
    </AbsoluteFill>
  );
};

const Features: React.FC = () => {
  const frame = useCurrentFrame();
  const items = [
    "watch them think, live",
    "attach images & PDFs",
    "sees each tool's skills",
    "git-worktree isolation",
    "you approve before it runs",
  ];
  return (
    <AbsoluteFill style={{ justifyContent: "center", alignItems: "center" }}>
      <div style={{ display: "flex", flexDirection: "column", gap: 22 }}>
        {items.map((t, i) => {
          const r = reveal(frame, i * 9, 16);
          return (
            <div
              key={i}
              style={{
                opacity: r.opacity,
                transform: `translateX(${(1 - r.opacity) * -24}px)`,
                fontFamily: mono,
                fontSize: 38,
                color: C.text,
              }}
            >
              <span style={{ color: C.accent }}>›</span> {t}
            </div>
          );
        })}
      </div>
    </AbsoluteFill>
  );
};

const Outro: React.FC = () => {
  const frame = useCurrentFrame();
  const a = reveal(frame, 6, 22);
  return (
    <AbsoluteFill style={{ justifyContent: "center", alignItems: "center" }}>
      <div style={{ opacity: a.opacity, transform: `translateY(${a.y}px)`, textAlign: "center" }}>
        <div style={{ fontFamily: mono, fontSize: 90, fontWeight: 800 }}>
          <span style={{ color: C.accent }}>❯</span> <span style={{ color: C.text }}>tales</span>
        </div>
        <div style={{ marginTop: 24, fontFamily: mono, fontSize: 30, color: C.dim }}>
          github.com/cesarfrancots/tales
        </div>
        <div style={{ marginTop: 12, fontFamily: sans, fontSize: 22, color: C.faint }}>
          open source · MIT
        </div>
      </div>
    </AbsoluteFill>
  );
};

export const TalesDemo: React.FC = () => {
  return (
    <AbsoluteFill style={{ background: C.bg }}>
      <Bg />
      <Sequence durationInFrames={96}>
        <Fade>
          <Intro />
        </Fade>
      </Sequence>
      <Sequence from={96} durationInFrames={330}>
        <Fade>
          <Collab />
        </Fade>
      </Sequence>
      <Sequence from={426} durationInFrames={114}>
        <Fade>
          <Gate />
        </Fade>
      </Sequence>
      <Sequence from={540} durationInFrames={120}>
        <Fade>
          <Execute />
        </Fade>
      </Sequence>
      <Sequence from={660} durationInFrames={84}>
        <Fade>
          <Features />
        </Fade>
      </Sequence>
      <Sequence from={744} durationInFrames={66}>
        <Fade>
          <Outro />
        </Fade>
      </Sequence>
    </AbsoluteFill>
  );
};
