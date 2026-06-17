import { loadFont as loadMono } from "@remotion/google-fonts/JetBrainsMono";
import { loadFont as loadSans } from "@remotion/google-fonts/Inter";

export const mono = loadMono().fontFamily;
export const sans = loadSans().fontFamily;

// Matches the Tales TUI / web palette.
export const C = {
  bg: "#0b0d11",
  bg2: "#0d1016",
  panel: "#11151b",
  border: "#1b2029",
  text: "#d2d8e2",
  dim: "#6b7483",
  faint: "#3c434f",
  claude: "#5cb0ff",
  codex: "#c08cff",
  you: "#7ee0a3",
  accent: "#2dd4bf",
};
