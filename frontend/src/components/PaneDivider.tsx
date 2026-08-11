'use client';

import React, { useEffect, useRef, useState } from 'react';
import { PANES, PaneKey, paintPaneWidth, setPaneWidth } from '@/lib/panes';
import { cn } from '@/lib/utils';

/** Arrow-key increment. One press should be visible without being a jump. */
const KEY_STEP = 16;

const clamp = (v: number, min: number, max: number) => Math.min(Math.max(v, min), max);

/**
 * Drag handle on a pane's trailing edge. It straddles the hairline the pane
 * already draws — nothing at rest, a brand rule while grabbed — so the layout
 * gains an affordance without gaining a line.
 *
 * Absolutely positioned: the parent must be `relative` and own the border. The
 * pane's rendered box is the single source of truth for its current width, so
 * the default never has to be repeated outside globals.css.
 */
export function PaneDivider({ pane, label }: { pane: PaneKey; label: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const drag = useRef<{ x: number; from: number; min: number; max: number } | null>(null);
  /** Last width the pointer asked for — persisted, so intent survives a CSS clamp. */
  const requested = useRef(0);
  const [dragging, setDragging] = useState(false);
  /** Mirrors the rendered width for assistive tech only; 0 until measured. */
  const [width, setWidth] = useState(0);

  const measure = () => ref.current?.parentElement?.getBoundingClientRect() ?? null;

  useEffect(() => {
    const rect = measure();
    if (rect) setWidth(Math.round(rect.width));
  }, []);

  /**
   * Upper bound is whichever is tighter: the pane's own ceiling, or what the
   * window can spare once the panes after it keep their reserve.
   */
  const boundsFor = (rect: DOMRect) => {
    const { min, max, reserve } = PANES[pane];
    return { min, max: Math.max(min, Math.min(max, window.innerWidth - rect.left - reserve)) };
  };

  const commit = (px: number) => {
    setPaneWidth(pane, px);
    setWidth(Math.round(px));
  };

  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    const rect = measure();
    if (!rect) return;

    // Capture so the pointer can leave the 5px target — and so a drag across
    // the transcript never turns into a text selection.
    e.currentTarget.setPointerCapture(e.pointerId);
    requested.current = rect.width;
    drag.current = { x: e.clientX, from: rect.width, ...boundsFor(rect) };
    setDragging(true);
  };

  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d) return;
    requested.current = clamp(d.from + e.clientX - d.x, d.min, d.max);
    paintPaneWidth(pane, requested.current);
  };

  const endDrag = () => {
    if (!drag.current) return;
    drag.current = null;
    setDragging(false);
    commit(requested.current);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    const step = e.key === 'ArrowLeft' ? -KEY_STEP : e.key === 'ArrowRight' ? KEY_STEP : 0;
    if (!step) return;
    const rect = measure();
    if (!rect) return;

    e.preventDefault();
    const { min, max } = boundsFor(rect);
    commit(clamp(rect.width + step, min, max));
  };

  // Back to the default in globals.css, not to a number duplicated here.
  const reset = () => {
    setPaneWidth(pane, null);
    requestAnimationFrame(() => {
      const rect = measure();
      if (rect) setWidth(Math.round(rect.width));
    });
  };

  return (
    <div
      ref={ref}
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      aria-valuenow={width || undefined}
      aria-valuemin={PANES[pane].min}
      aria-valuemax={PANES[pane].max}
      tabIndex={0}
      title={`${label} — drag, or double-click to reset`}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      onKeyDown={onKeyDown}
      onDoubleClick={reset}
      className="group absolute -right-[3px] top-0 z-sticky h-full w-[5px] cursor-col-resize touch-none select-none"
    >
      {/* 1px, inset inside the hit target, so the hairline underneath never
          reads as thicker than the app's other rules. */}
      <span
        aria-hidden
        className={cn(
          'pointer-events-none absolute inset-y-0 left-[2px] w-px transition-colors duration-fast',
          dragging ? 'bg-brand' : 'bg-transparent group-hover:bg-brand/60'
        )}
      />
    </div>
  );
}
