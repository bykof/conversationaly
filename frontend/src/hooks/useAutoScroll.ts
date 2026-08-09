import { useRef, useState, useEffect, useCallback, RefObject } from "react";
import { Virtualizer } from "@tanstack/react-virtual";

interface UseAutoScrollProps {
    scrollRef: RefObject<HTMLDivElement | null>;
    segments: any[];
    isRecording: boolean;
    isPaused: boolean;
    activeSegmentId?: string;
    virtualizer?: Virtualizer<HTMLDivElement, Element>;
    virtualizationThreshold?: number;
    disableAutoScroll?: boolean; // Completely disable auto-scroll behavior (for meeting details page)
    /**
     * The volatile streaming tail rendered under the last committed segment.
     *
     * It has to be here because it grows the page without adding a segment, and
     * following only `segments.length` left live text stranded below the fold.
     */
    liveText?: string;
}

interface UseAutoScrollReturn {
    autoScroll: boolean;
    setAutoScroll: (value: boolean) => void;
    scrollToBottom: () => void;
}

// Threshold in pixels to consider "at the bottom"
const SCROLL_THRESHOLD = 100;

/**
 * Custom hook to manage auto-scrolling behavior for transcript
 *
 * Features:
 * - Auto-scrolls to bottom when new content arrives during recording
 * - Pauses auto-scroll when user manually scrolls up
 * - Resumes auto-scroll when user scrolls back to the bottom
 *
 * @param segments - Array of transcript segments
 * @param isRecording - Whether recording is in progress
 * @param isPaused - Whether recording is paused
 * @param activeSegmentId - ID of the currently active segment
 * @returns Scroll ref, auto-scroll state, and scroll control functions
 */
export function useAutoScroll({
    scrollRef,
    segments,
    isRecording,
    isPaused,
    activeSegmentId,
    virtualizer,
    virtualizationThreshold = 10,
    disableAutoScroll = false,
    liveText = '',
}: UseAutoScrollProps): UseAutoScrollReturn {
    const useVirtualization = virtualizer && segments.length >= virtualizationThreshold;
    const [autoScroll, setAutoScroll] = useState(true);
    // Ref to always have current autoScroll value in effects
    const autoScrollRef = useRef(autoScroll);
    autoScrollRef.current = autoScroll;

    // Track if we're doing a programmatic scroll
    const isProgrammaticScrollRef = useRef(false);

    /**
     * Check if the user is scrolled near the bottom
     */
    const isNearBottom = useCallback(() => {
        if (!scrollRef.current) return true;
        const { scrollTop, scrollHeight, clientHeight } = scrollRef.current;
        return scrollHeight - scrollTop - clientHeight <= SCROLL_THRESHOLD;
    }, [scrollRef]);

    /**
     * Scroll to bottom programmatically
     */
    const scrollToBottom = useCallback(() => {
        if (scrollRef.current) {
            isProgrammaticScrollRef.current = true;
            scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
            setAutoScroll(true);

            // Reset the flag after a small delay to account for scroll event propagation
            setTimeout(() => {
                isProgrammaticScrollRef.current = false;
            }, 50);
        }
    }, [scrollRef]);

    // Handle scroll events to detect manual scrolling
    useEffect(() => {
        const container = scrollRef.current;
        if (!container) return;

        let scrollTimeout: ReturnType<typeof setTimeout> | null = null;

        const handleScroll = () => {
            // Skip if this is a programmatic scroll
            if (isProgrammaticScrollRef.current) {
                return;
            }

            // Debounce scroll handling to prevent rapid state changes
            if (scrollTimeout) {
                clearTimeout(scrollTimeout);
            }

            scrollTimeout = setTimeout(() => {
                // A real user scroll is the ONLY thing that decides whether we
                // keep following. Deriving that from geometry anywhere else
                // reads a page that is still growing (see below).
                setAutoScroll(isNearBottom());
            }, 100);
        };

        container.addEventListener("scroll", handleScroll, { passive: true });

        return () => {
            container.removeEventListener("scroll", handleScroll);
            if (scrollTimeout) {
                clearTimeout(scrollTimeout);
            }
        };
    }, [isNearBottom, scrollRef]);

    // Follow the transcript as it grows, for as long as the user wants us to.
    //
    // This used to re-check isNearBottom() here and bail out when it was false,
    // which is the bug that made live transcription look broken. By the time
    // this effect runs the new content is already in the DOM, so the container
    // is *already* further than SCROLL_THRESHOLD from the bottom — that is the
    // whole reason we are about to scroll. Two segments landing between paints
    // was enough to fail the check, and once it failed the view never moved
    // again, so nothing generated a scroll event, so nothing ever re-armed it.
    // The transcript kept arriving and kept rendering; it was just permanently
    // below the fold.
    //
    // Whether to follow is a question about the user's intent, and only the
    // user's own scrolling answers it. That lives in `autoScroll`, set by the
    // scroll handler above.
    useEffect(() => {
        if (disableAutoScroll) return;
        if (!autoScrollRef.current || !isRecording || isPaused) return;
        if (segments.length === 0 && !liveText) return;

        isProgrammaticScrollRef.current = true;

        if (useVirtualization && virtualizer) {
            // Large offset rather than the last index: the live tail is rendered
            // outside the virtualizer, so the true bottom is past its total size.
            virtualizer.scrollToOffset(virtualizer.getTotalSize() + 1000, { align: "end" });
        }
        // Always settle with a direct scrollTop after layout: it is the only
        // measurement that accounts for both virtual rows and the live tail.
        const settle = setTimeout(() => {
            if (scrollRef.current) {
                scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
            }
            isProgrammaticScrollRef.current = false;
        }, 50);

        return () => clearTimeout(settle);
    }, [
        segments.length,
        liveText,
        autoScroll,
        isRecording,
        isPaused,
        useVirtualization,
        virtualizer,
        scrollRef,
        disableAutoScroll,
    ]);

    // Auto-scroll to active segment (when clicking on search results, etc.)
    useEffect(() => {
        if (activeSegmentId) {
            isProgrammaticScrollRef.current = true;

            if (useVirtualization && virtualizer) {
                const index = segments.findIndex((s: any) => s.id === activeSegmentId);
                if (index >= 0) {
                    virtualizer.scrollToIndex(index, { align: "center", behavior: "smooth" });
                }
            } else {
                const element = document.getElementById(`segment-${activeSegmentId}`);
                if (element) {
                    element.scrollIntoView({ behavior: "smooth", block: "center" });
                }
            }

            // Reset the flag after scroll animation completes
            setTimeout(() => {
                isProgrammaticScrollRef.current = false;
            }, 500);
        }
    }, [activeSegmentId, useVirtualization, virtualizer, segments]);

    return {
        autoScroll,
        setAutoScroll,
        scrollToBottom,
    };
}
