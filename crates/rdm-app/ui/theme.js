// Theme colours on top of the black and white defaults. Every other colour
// (text, lines, panels) is derived from the background, so any pick stays
// readable. Shared by the main window and the menu bar panel.

const THEME_PRESETS = [
  { name: 'System', background: null, primary: null },
  { name: 'Paper', background: '#fafaf9', primary: '#0a0a0a' },
  { name: 'Carbon', background: '#09090b', primary: '#fafafa' },
  { name: 'Midnight', background: '#0b1020', primary: '#7c9cff' },
  { name: 'Ocean', background: '#081517', primary: '#2dd4bf' },
  { name: 'Forest', background: '#0b120d', primary: '#4ade80' },
  { name: 'Ember', background: '#130e0b', primary: '#fb923c' },
  { name: 'Rose', background: '#fdf6f7', primary: '#e11d48' },
  { name: 'Lavender', background: '#f7f6fd', primary: '#6d28d9' },
  { name: 'Sand', background: '#f5f0e6', primary: '#9a3412' },
  { name: 'Nord', background: '#2e3440', primary: '#88c0d0' },
  { name: 'Dracula', background: '#282a36', primary: '#bd93f9' },
  { name: 'Tokyo Night', background: '#1a1b26', primary: '#7aa2f7' },
  { name: 'Catppuccin', background: '#1e1e2e', primary: '#f5c2e7' },
  { name: 'Gruvbox', background: '#282828', primary: '#fabd2f' },
  { name: 'Solarized', background: '#002b36', primary: '#2aa198' },
  { name: 'Monokai', background: '#272822', primary: '#a6e22e' },
  { name: 'Rosé Pine', background: '#191724', primary: '#ebbcba' },
  { name: 'Latte', background: '#eff1f5', primary: '#8839ef' },
  { name: 'Solar Light', background: '#fdf6e3', primary: '#268bd2' },
  { name: 'Mint', background: '#f2faf6', primary: '#059669' },
  { name: 'Sky', background: '#f3f8fd', primary: '#0284c7' },
];

const THEME_VARS = ['--bg', '--panel', '--raise', '--line', '--line-strong', '--text', '--muted', '--faint', '--hover',
  '--selected', '--solid', '--solid-text', '--accent', '--cell-left', '--shadow', 'color-scheme'];

const isHex = (c) => typeof c === 'string' && /^#[0-9a-f]{6}$/i.test(c);

function rgbOf(hex) {
  const n = parseInt(hex.slice(1), 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

const hexOf = (rgb) => `#${rgb.map((v) => Math.round(v).toString(16).padStart(2, '0')).join('')}`;

/** `a` moved `t` of the way towards `b`. */
function mixHex(a, b, t) {
  const [x, y] = [rgbOf(a), rgbOf(b)];
  return hexOf(x.map((v, i) => v + (y[i] - v) * t));
}

/** WCAG relative luminance, 0 (black) to 1 (white). */
function luminance(hex) {
  const [r, g, b] = rgbOf(hex).map((v) => {
    const c = v / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

const isDark = (hex) => luminance(hex) < 0.18;

/** Near-black or near-white, whichever reads better on `hex`. */
const inkOn = (hex) => (isDark(hex) ? '#fafafa' : '#0a0a0a');

/** The CSS variables for a theme; only what it overrides. */
function themeVars(theme) {
  const vars = {};
  const bg = isHex(theme?.background) ? theme.background : null;
  const primary = isHex(theme?.primary) ? theme.primary : null;
  if (bg) {
    const dark = isDark(bg);
    const text = inkOn(bg);
    const ink = dark ? '255, 255, 255' : '10, 10, 10';
    Object.assign(vars, {
      'color-scheme': dark ? 'dark' : 'light',
      '--bg': bg,
      '--panel': mixHex(bg, text, dark ? 0.025 : 0.03),
      '--raise': dark ? mixHex(bg, '#ffffff', 0.05) : mixHex(bg, '#ffffff', 0.7),
      '--text': text,
      '--muted': mixHex(text, bg, 0.45),
      '--faint': mixHex(text, bg, 0.7),
      '--line': `rgba(${ink}, ${dark ? 0.08 : 0.09})`,
      '--line-strong': `rgba(${ink}, ${dark ? 0.15 : 0.17})`,
      '--hover': `rgba(${ink}, 0.04)`,
      '--selected': `rgba(${ink}, 0.075)`,
      '--cell-left': `rgba(${ink}, ${dark ? 0.08 : 0.09})`,
      '--solid': text,
      '--solid-text': bg,
      '--shadow': dark
        ? '0 0 0 1px rgba(255, 255, 255, 0.06), 0 24px 64px -12px rgba(0, 0, 0, 0.7)'
        : '0 1px 0 rgba(0, 0, 0, 0.02), 0 24px 64px -12px rgba(0, 0, 0, 0.22)',
    });
  }
  if (primary) {
    Object.assign(vars, { '--solid': primary, '--solid-text': inkOn(primary), '--accent': primary });
  }
  return vars;
}

function applyTheme(theme) {
  const style = document.documentElement.style;
  for (const v of THEME_VARS) style.removeProperty(v);
  for (const [k, v] of Object.entries(themeVars(theme))) style.setProperty(k, v);
}
