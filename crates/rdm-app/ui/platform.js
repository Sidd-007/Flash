// What differs between macOS and Windows in the UI: the shortcut key, a few
// words, and path separators. Shared by the main window and the panel.

const IS_MAC = /Mac/.test(navigator.platform || navigator.userAgent);
document.documentElement.dataset.platform = IS_MAC ? 'mac' : 'windows';

/** Whether the platform's command key is held: ⌘ on macOS, Ctrl on Windows. */
const modKey = (e) => (IS_MAC ? e.metaKey : e.ctrlKey);

/** "⌘K" on macOS, "Ctrl+K" on Windows. */
const keyLabel = (s) => (IS_MAC ? s : s.replace(/⌘/g, 'Ctrl+'));

const FILE_MANAGER = IS_MAC ? 'Finder' : 'Explorer';

/** The last part of a path, with either separator. */
const baseName = (path) => path.split(/[\\/]/).filter(Boolean).pop() || path;

/** The folder a file is in. */
const dirName = (path) => path.replace(/[\\/][^\\/]*$/, '');

/** Joins a folder and a name with the folder's own separator. */
const joinPath = (dir, name) => `${dir}${dir.includes('\\') ? '\\' : '/'}${name}`;

/**
 * Adapts static markup: shortcut hints in titles and <kbd>s, and any element
 * with a `data-win` attribute takes that text on Windows.
 */
function localizePlatform(root = document) {
  if (IS_MAC) return;
  for (const el of root.querySelectorAll('[title*="⌘"]')) el.title = keyLabel(el.title);
  for (const el of root.querySelectorAll('kbd, .palette-foot span')) {
    if (el.textContent.includes('⌘')) el.textContent = keyLabel(el.textContent);
  }
  for (const el of root.querySelectorAll('[data-win]')) el.textContent = el.dataset.win;
}
