import React from 'react';
import { Dialog, DialogContent, DialogTitle, DialogTrigger } from './ui/dialog';
import { VisuallyHidden } from './ui/visually-hidden';
import { About } from './About';
import { cn } from '@/lib/utils';

/**
 * The mark is an aperture: a closed ring with a single gap, solid centre. It
 * reads as the "C" monogram and as a record button, and the closed loop is the
 * privacy claim — audio goes in, nothing leaves. The centre dot turns red while
 * recording, so the brand mark *is* the live indicator rather than a decoration
 * sitting next to one.
 *
 * Same geometry as src-tauri/app-icon.svg, which generates the app icons. Edit
 * both, or the dock and the sidebar drift apart.
 */
export function Mark({
  live = false,
  className,
}: {
  live?: boolean;
  className?: string;
}) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      aria-hidden="true"
      className={cn('h-5 w-5', className)}
    >
      <path
        d="M18.28 6.73A8.2 8.2 0 1 0 18.28 17.27"
        stroke="currentColor"
        strokeWidth={2.8}
        strokeLinecap="round"
        fill="none"
      />
      <circle
        cx={12}
        cy={12}
        r={3.4}
        className={cn(live ? 'fill-danger' : 'fill-current', live && 'animate-live')}
        style={{ transformOrigin: '12px 12px' }}
      />
    </svg>
  );
}

export function Wordmark({ className }: { className?: string }) {
  return (
    <span
      className={cn(
        'font-semibold tracking-[-0.018em] text-ink whitespace-nowrap',
        className
      )}
    >
      Conversationaly
    </span>
  );
}

interface LogoProps {
  isCollapsed: boolean;
  live?: boolean;
}

/** Brand lockup in the rail. Opens About. */
const Logo = React.forwardRef<HTMLButtonElement, LogoProps>(
  ({ isCollapsed, live = false }, ref) => (
    <Dialog aria-describedby={undefined}>
      <DialogTrigger asChild>
        <button
          ref={ref}
          title="About Conversationaly"
          className={cn(
            'group flex items-center rounded-md text-ink transition-colors duration-fast',
            'hover:bg-ink/5 active:bg-ink/10',
            // Same hit box and same left axis as every other rail row: the mark
            // sits at 2×--rail-gutter and the wordmark where a route label does.
            isCollapsed ? 'h-8 w-8 justify-center' : 'h-8 w-full gap-2 px-gutter'
          )}
        >
          <Mark live={live} className="h-4 w-4 shrink-0 text-brand" />
          {!isCollapsed && <Wordmark className="text-base" />}
          <VisuallyHidden>About Conversationaly</VisuallyHidden>
        </button>
      </DialogTrigger>
      <DialogContent className="max-w-lg">
        <VisuallyHidden>
          <DialogTitle>About Conversationaly</DialogTitle>
        </VisuallyHidden>
        <About />
      </DialogContent>
    </Dialog>
  )
);

Logo.displayName = 'Logo';

export default Logo;
