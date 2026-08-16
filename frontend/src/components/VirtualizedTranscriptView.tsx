'use client';

import { useRef, useReducer, startTransition, useEffect, memo } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useAutoScroll } from "@/hooks/useAutoScroll";
import { ConfidenceIndicator } from "./ConfidenceIndicator";
import { Tooltip, TooltipContent, TooltipTrigger } from "./ui/tooltip";
import { RecordingStatusBar } from "./RecordingStatusBar";
import { TranscriptSegmentData } from "@/types";
import { Loader2, Mic } from "lucide-react";
import { cn } from "@/lib/utils";

export interface VirtualizedTranscriptViewProps {
    /** Transcript segments to display */
    segments: TranscriptSegmentData[];
    /** Whether recording is in progress */
    isRecording?: boolean;
    /** Whether recording is paused */
    isPaused?: boolean;
    /** Whether processing/finalizing transcription */
    isProcessing?: boolean;
    /** Whether stopping */
    isStopping?: boolean;
    /** Uncommitted live text from a streaming model; shown dimmed below the segments */
    partialText?: string;
    /** Show confidence indicators */
    showConfidence?: boolean;
    /** Completely disable auto-scroll behavior (for meeting details page) */
    disableAutoScroll?: boolean;

    // Pagination props (infinite scroll)
    hasMore?: boolean;
    isLoadingMore?: boolean;
    totalCount?: number;
    loadedCount?: number;
    onLoadMore?: () => void;
}

// Threshold for enabling virtualization (below this, use simple rendering)
const VIRTUALIZATION_THRESHOLD = 10;

// Helper function to format seconds as recording-relative time [MM:SS]
function formatRecordingTime(seconds: number | undefined): string {
    if (seconds === undefined) return '--:--';

    const totalSeconds = Math.floor(seconds);
    const hours = Math.floor(totalSeconds / 3600);
    const minutes = Math.floor((totalSeconds % 3600) / 60);
    const secs = totalSeconds % 60;
    const pad = (n: number) => n.toString().padStart(2, '0');

    // Brackets were doing the work of "this is a timestamp"; the mono gutter
    // does that now. Meetings run past an hour, so carry hours when needed.
    return hours > 0 ? `${hours}:${pad(minutes)}:${pad(secs)}` : `${pad(minutes)}:${pad(secs)}`;
}

// A filler-word stripper used to run here, deleting uh/um/er/ah/hmm/hm/eh/oh
// from every line before rendering. It was wrong in two ways at once: the words
// are real words in the languages this app transcribes ("er" = he, "oh", "eh",
// "um" are ordinary German), and it rewrote only the *displayed* text, so the
// transcript on screen silently disagreed with the one that got saved,
// exported and summarised. A transcript that edits what was said is not a
// transcript. Render what the model heard.

// Memoized transcript segment component
const TranscriptSegment = memo(function TranscriptSegment({
    id,
    timestamp,
    text,
    confidence,
    showConfidence,
}: {
    id: string;
    timestamp: number;
    text: string;
    confidence?: number;
    showConfidence: boolean;
}) {
    const isSilence = text.trim() === '';
    const displayText = isSilence ? 'Silence' : text;

    return (
        <div id={`segment-${id}`} className="group flex items-baseline gap-3 py-1.5">
            {/* Timestamp gutter — a machine fact, so it is set in mono. */}
            <Tooltip>
                <TooltipTrigger asChild>
                    <span className="readout w-[3.25rem] shrink-0 select-none text-2xs text-ink-faint transition-colors duration-fast group-hover:text-ink-muted">
                        {formatRecordingTime(timestamp)}
                    </span>
                </TooltipTrigger>
                <TooltipContent side="left">
                    {confidence !== undefined && showConfidence ? (
                        <span className="flex items-center gap-1.5">
                            Decode confidence
                            <ConfidenceIndicator confidence={confidence} always />
                        </span>
                    ) : (
                        'Position in recording'
                    )}
                </TooltipContent>
            </Tooltip>

            <p
                className={cn(
                    'min-w-0 flex-1 text-md leading-relaxed',
                    isSilence ? 'italic text-ink-faint' : 'text-ink'
                )}
            >
                {displayText}
                {confidence !== undefined && showConfidence && (
                    <>
                        {' '}
                        <ConfidenceIndicator confidence={confidence} showIndicator />
                    </>
                )}
            </p>
        </div>
    );
});

export const VirtualizedTranscriptView: React.FC<VirtualizedTranscriptViewProps> = ({
    segments,
    isRecording = false,
    isPaused = false,
    isProcessing = false,
    isStopping = false,
    partialText = '',
    showConfidence = true,
    disableAutoScroll = false,
    hasMore = false,
    isLoadingMore = false,
    totalCount = 0,
    loadedCount = 0,
    onLoadMore,
}) => {
    // Create scroll ref first - shared between virtualizer and auto-scroll hook
    const scrollRef = useRef<HTMLDivElement>(null);
    // Ref for infinite scroll trigger element
    const loadMoreTriggerRef = useRef<HTMLDivElement>(null);

    // Force re-render without flushSync (avoids React warning)
    const [, rerender] = useReducer((x: number) => x + 1, 0);

    // Setup virtualizer for efficient rendering of large lists
    const virtualizer = useVirtualizer({
        // `virtualizer.measureElement` is attached as a ref (below), so react-virtual's
        // internal re-render notify fires during React's commit phase. Its default
        // path wraps that in flushSync, which React rejects with "flushSync was
        // called from inside a lifecycle method". Same for `_willUpdate` notifying
        // while `isScrolling`. Batched re-render is fine here — heights settle on
        // the next paint.
        useFlushSync: false,
        count: segments.length,
        getScrollElement: () => scrollRef.current,
        estimateSize: () => 60, // Estimated height per segment
        overscan: 10, // Render extra items above/below viewport
        onChange: () => {
            startTransition(() => {
                rerender();
            });
        },
    });

    // Custom hook for auto-scrolling (supports both virtualized and non-virtualized)
    useAutoScroll({
        scrollRef,
        segments,
        isRecording,
        isPaused,
        virtualizer,
        virtualizationThreshold: VIRTUALIZATION_THRESHOLD,
        disableAutoScroll,
        liveText: partialText,
    });

    // Infinite scroll: IntersectionObserver to trigger loading more
    useEffect(() => {
        if (!onLoadMore || !hasMore || isLoadingMore || isRecording || segments.length === 0) {
            return;
        }

        const triggerElement = loadMoreTriggerRef.current;
        if (!triggerElement) return;

        const observer = new IntersectionObserver(
            (entries) => {
                if (entries[0].isIntersecting && hasMore && !isLoadingMore) {
                    onLoadMore();
                }
            },
            {
                root: null,
                rootMargin: '100px',
                threshold: 0,
            }
        );

        observer.observe(triggerElement);

        return () => observer.disconnect();
    }, [hasMore, isLoadingMore, onLoadMore, isRecording, segments.length]);

    // Scroll-based fallback for fast scrolling
    useEffect(() => {
        if (!onLoadMore || !hasMore || isLoadingMore || isRecording) return;

        const scrollElement = scrollRef.current;
        if (!scrollElement) return;

        let ticking = false;

        const handleScroll = () => {
            if (ticking || isLoadingMore || !hasMore) return;

            ticking = true;
            requestAnimationFrame(() => {
                const { scrollTop, scrollHeight, clientHeight } = scrollElement;
                const scrollBottom = scrollHeight - scrollTop - clientHeight;

                // Trigger load when within 200px of bottom
                if (scrollBottom < 200 && hasMore && !isLoadingMore) {
                    onLoadMore();
                }
                ticking = false;
            });
        };

        scrollElement.addEventListener('scroll', handleScroll, { passive: true });
        return () => scrollElement.removeEventListener('scroll', handleScroll);
    }, [onLoadMore, hasMore, isLoadingMore, isRecording]);

    // Use simple rendering for small lists, virtualization for large lists
    const useVirtualization = segments.length >= VIRTUALIZATION_THRESHOLD;

    // What sits below the last committed segment while recording: the streaming
    // decoder's uncommitted tail if it has one, otherwise the Listening pulse.
    // Outside the virtualizer — this text is rewritten several times a second
    // and re-measuring a virtual row on every keystroke-sized change thrashes.
    const liveTail =
        !isStopping && isRecording && !isPaused && !isProcessing ? (
            partialText ? (
                <div className="mt-2 flex items-baseline gap-3 py-1.5 pl-[4.25rem] animate-fade-in">
                    <p className="min-w-0 flex-1 text-md leading-relaxed text-ink-muted">
                        {partialText}
                    </p>
                </div>
            ) : segments.length > 0 ? (
                <div className="mt-4 flex items-center gap-2 pl-[4.25rem] animate-fade-in">
                    <span aria-hidden className="h-1.5 w-1.5 rounded-full bg-danger animate-live" />
                    <span className="text-sm text-ink-muted">Listening</span>
                </div>
            ) : null
        ) : null;

    return (
        <div ref={scrollRef} className="scrollbar-slim flex h-full flex-col overflow-y-auto px-4 py-2">
            {/* Recording Status Bar - Sticky at top, always visible when recording */}
            {isRecording && (
                <div className="sticky top-0 z-sticky bg-canvas pb-2">
                    <RecordingStatusBar isPaused={isPaused} />
                </div>
            )}

            {/* Content - add padding when recording to prevent overlap */}
            <div className={isRecording ? 'pt-2' : ''}>
            {/* A partial with no committed segments yet is still text on screen —
                showing "Listening" underneath it would contradict itself. */}
            {segments.length === 0 && !partialText ? (
                // Empty states teach the next action rather than saying "nothing here".
                <div className="flex min-h-[55vh] flex-col items-center justify-center px-6 text-center animate-fade-in">
                    {isRecording ? (
                        <>
                            <span
                                aria-hidden
                                className={cn(
                                    'mb-3 h-2.5 w-2.5 rounded-full',
                                    isPaused ? 'bg-warn' : 'bg-danger animate-live'
                                )}
                            />
                            <p className="text-md font-medium text-ink">
                                {isPaused ? 'Recording paused' : 'Listening'}
                            </p>
                            <p className="mt-1 max-w-[34ch] text-base leading-relaxed text-ink-muted">
                                {isPaused
                                    ? 'Resume from the transport below to keep capturing.'
                                    : 'Speech appears here a few seconds after it is spoken.'}
                            </p>
                        </>
                    ) : (
                        <>
                            <span className="mb-3 flex h-10 w-10 items-center justify-center rounded-lg bg-sunken text-ink-faint">
                                <Mic className="h-4.5 w-4.5" aria-hidden />
                            </span>
                            <p className="text-md font-medium text-ink">No transcript yet</p>
                            <p className="mt-1 max-w-[38ch] text-base leading-relaxed text-ink-muted">
                                Start a recording and speech is transcribed here live — on this
                                machine, with no audio leaving it.
                            </p>
                        </>
                    )}
                </div>
            ) : useVirtualization ? (
                // Virtualized rendering for large lists
                <>
                    <div
                        style={{
                            height: virtualizer.getTotalSize(),
                            width: "100%",
                            position: "relative",
                        }}
                    >
                        {virtualizer.getVirtualItems().map((virtualRow) => {
                            const segment = segments[virtualRow.index];

                            return (
                                <div
                                    key={segment.id}
                                    data-index={virtualRow.index}
                                    ref={virtualizer.measureElement}
                                    style={{
                                        position: "absolute",
                                        top: 0,
                                        left: 0,
                                        width: "100%",
                                        transform: `translateY(${virtualRow.start}px)`,
                                    }}
                                >
                                    <TranscriptSegment
                                        id={segment.id}
                                        timestamp={segment.timestamp}
                                        text={segment.text}
                                        confidence={segment.confidence}
                                        showConfidence={showConfidence}
                                    />
                                </div>
                            );
                        })}
                    </div>

                    {/* Infinite scroll trigger and loading indicator */}
                    {(hasMore || isLoadingMore) && !isRecording && segments.length > 0 && (
                        <div ref={loadMoreTriggerRef} className="mt-2 flex items-center justify-center py-4">
                            {isLoadingMore ? (
                                <div className="flex items-center gap-2 text-ink-muted">
                                    <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                                    <span className="text-sm">Loading more…</span>
                                </div>
                            ) : hasMore && totalCount > 0 ? (
                                <span className="readout text-2xs text-ink-faint">
                                    {loadedCount} / {totalCount} segments
                                </span>
                            ) : null}
                        </div>
                    )}

                    {liveTail}
                </>
            ) : (
                // Simple rendering for small lists (better animations)
                <>
                    <div>
                        {segments.map((segment) => (
                            // CSS keyframe rather than framer-motion: the reveal must
                            // survive a headless render and a hidden tab.
                            <div key={segment.id} className="animate-segment-in">
                                <TranscriptSegment
                                    id={segment.id}
                                    timestamp={segment.timestamp}
                                    text={segment.text}
                                    confidence={segment.confidence}
                                    showConfidence={showConfidence}
                                />
                            </div>
                        ))}
                    </div>

                    {/* Infinite scroll trigger (for small lists that grow) */}
                    {(hasMore || isLoadingMore) && !isRecording && segments.length > 0 && (
                        <div ref={loadMoreTriggerRef} className="mt-2 flex items-center justify-center py-4">
                            {isLoadingMore ? (
                                <div className="flex items-center gap-2 text-ink-muted">
                                    <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                                    <span className="text-sm">Loading more…</span>
                                </div>
                            ) : hasMore && totalCount > 0 ? (
                                <span className="readout text-2xs text-ink-faint">
                                    {loadedCount} / {totalCount} segments
                                </span>
                            ) : null}
                        </div>
                    )}

                    {liveTail}
                </>
            )}
            </div>
        </div>
    );
};
