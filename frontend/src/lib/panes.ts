/**
 * User-set pane widths.
 *
 * The rail's *collapse* is deliberately not persisted — it is a per-session
 * gesture. A *width* is the opposite: the user fits the app to their window
 * once and expects it to stay, so it is stored and restored before first paint
 * by `PANES_INIT_SCRIPT`, the same trick the theme uses.
 *
 * Default widths live in `globals.css` and stay there. This table only bounds
 * the drag; resetting a pane removes the property and the CSS default returns.
 * CSS also carries the safety net for a shrinking window (`min()` on the rail,
 * `max-width` on the transcript pane), so no window size can let one pane
 * starve the one after it.
 */

const STORAGE_KEY = 'conversationaly.panes';

export const PANES = {
  rail: {
    cssVar: '--rail-w',
    min: 224,
    max: 416,
    /** Width the panes after this one need to stay usable. */
    reserve: 420,
  },
  transcript: {
    cssVar: '--pane-transcript',
    min: 240,
    max: 720,
    reserve: 320,
  },
} as const;

export type PaneKey = keyof typeof PANES;

/** Generated from PANES so the pre-paint script cannot drift from the table. */
export const PANES_INIT_SCRIPT = `(function(){try{
var p=JSON.parse(localStorage.getItem('${STORAGE_KEY}')||'{}');
var s=document.documentElement.style;
${Object.entries(PANES)
  .map(([key, { cssVar }]) => `if(p.${key})s.setProperty('${cssVar}',p.${key}+'px');`)
  .join('\n')}
}catch(e){}})();`;

function read(): Partial<Record<PaneKey, number>> {
  try {
    return JSON.parse(localStorage.getItem(STORAGE_KEY) ?? '{}');
  } catch {
    return {};
  }
}

/**
 * Writes the width for the current frame only. Used on every pointer move:
 * routing a drag through React state would re-render the virtualized
 * transcript on each frame, and /DESIGN.md is explicit that reporting state
 * may not cost a relayout of that list.
 */
export function paintPaneWidth(pane: PaneKey, px: number) {
  document.documentElement.style.setProperty(PANES[pane].cssVar, `${Math.round(px)}px`);
}

/** Applies a width and remembers it. `null` restores the CSS default. */
export function setPaneWidth(pane: PaneKey, px: number | null) {
  const stored = read();

  if (px === null) {
    delete stored[pane];
    document.documentElement.style.removeProperty(PANES[pane].cssVar);
  } else {
    stored[pane] = Math.round(px);
    paintPaneWidth(pane, px);
  }

  localStorage.setItem(STORAGE_KEY, JSON.stringify(stored));
}
