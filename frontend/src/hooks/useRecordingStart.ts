import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTranscripts } from '@/contexts/TranscriptContext';
import { useSidebar } from '@/components/Sidebar/SidebarProvider';
import { useConfig } from '@/contexts/ConfigContext';
import { useRecordingState, RecordingStatus } from '@/contexts/RecordingStateContext';
import { recordingService } from '@/services/recordingService';
import { showRecordingNotification } from '@/lib/recordingNotification';
import { toast } from 'sonner';

interface UseRecordingStartReturn {
  handleRecordingStart: () => Promise<void>;
  isStarting: boolean;
}

/**
 * Custom hook for managing recording start lifecycle.
 * Handles both manual start (button click) and auto-start (from sidebar navigation).
 *
 * Features:
 * - Meeting title generation (format: Meeting DD_MM_YY_HH_MM_SS)
 * - Transcript clearing on start
 * - Recording notification display
 * - Auto-start from sidebar via sessionStorage flag
 * - A single `isStarting` flag covering all three start paths, so the record
 *   button can disable itself for the whole multi-second start instead of
 *   letting a second click fire a second `start_recording_*` invoke.
 */
export function useRecordingStart(
  isRecording: boolean,
  setIsRecording: (value: boolean) => void,
  showModal?: (name: 'modelSelector', message?: string) => void
): UseRecordingStartReturn {
  const [isStarting, setIsStarting] = useState(false);

  const { clearTranscripts, setMeetingTitle } = useTranscripts();
  const { setIsMeetingActive } = useSidebar();
  const { selectedDevices } = useConfig();
  const { setStatus } = useRecordingState();

  // Re-entrancy guard. This has to be a ref, not the `isStarting` state: two
  // clicks dispatched before React commits would both read the stale `false`.
  const isStartingRef = useRef(false);

  /**
   * Claim the start. Returns false when a start is already in flight, in which
   * case the caller must return without touching any state.
   *
   * Must be called as the first statement of a starter — before any `await`,
   * including the `checkParakeetReady()` one.
   */
  const beginStart = useCallback((): boolean => {
    if (isStartingRef.current) return false;
    isStartingRef.current = true;
    setIsStarting(true);
    return true;
  }, []);

  /** Release the start. Must run on every exit path, including errors. */
  const endStart = useCallback(() => {
    isStartingRef.current = false;
    setIsStarting(false);
  }, []);

  // Generate meeting title with timestamp
  const generateMeetingTitle = useCallback(() => {
    const now = new Date();
    const day = String(now.getDate()).padStart(2, '0');
    const month = String(now.getMonth() + 1).padStart(2, '0');
    const year = String(now.getFullYear()).slice(-2);
    const hours = String(now.getHours()).padStart(2, '0');
    const minutes = String(now.getMinutes()).padStart(2, '0');
    const seconds = String(now.getSeconds()).padStart(2, '0');
    return `Meeting ${day}_${month}_${year}_${hours}_${minutes}_${seconds}`;
  }, []);

  // Check if Parakeet transcription model is ready
  const checkParakeetReady = useCallback(async (): Promise<boolean> => {
    try {
      await invoke('transcribe_init');
      const hasModels = await invoke<boolean>('transcribe_has_available_models');
      return hasModels;
    } catch (error) {
      console.error('Failed to check Parakeet status:', error);
      return false;
    }
  }, []);

  // Check if any model is currently downloading
  const checkIfModelDownloading = useCallback(async (): Promise<boolean> => {
    try {
      const models = await invoke<any[]>('transcribe_get_available_models');
      const isDownloading = models.some(m =>
        m.status && (
          typeof m.status === 'object'
            ? 'Downloading' in m.status
            : m.status === 'Downloading'
        )
      );
      return isDownloading;
    } catch (error) {
      console.error('Failed to check model download status:', error);
      return false; // Default to not downloading (will show error + modal)
    }
  }, []);

  // Shared "model isn't ready" branch for all three start paths.
  const reportModelNotReady = useCallback(async () => {
    const isDownloading = await checkIfModelDownloading();
    if (isDownloading) {
      toast.info('Model download in progress', {
        description: 'Please wait for the transcription model to finish downloading before recording.',
        duration: 5000,
      });
    } else {
      toast.error('Transcription model not ready', {
        description: 'Please download a transcription model before recording.',
        duration: 5000,
      });
      showModal?.('modelSelector', 'Transcription model setup required');
    }
    setStatus(RecordingStatus.IDLE);
  }, [checkIfModelDownloading, showModal, setStatus]);

  // Handle manual recording start (from button click)
  const handleRecordingStart = useCallback(async () => {
    // First statement: claim the start before the first await, so a double
    // click cannot get two `start_recording_*` invokes past this point.
    if (!beginStart()) {
      console.log('Start already in flight, ignoring duplicate manual start');
      return;
    }

    try {
      console.log('handleRecordingStart called - checking Parakeet model status');

      // Check if Parakeet transcription model is ready before starting
      const parakeetReady = await checkParakeetReady();
      if (!parakeetReady) {
        await reportModelNotReady();
        return;
      }

      console.log('Parakeet ready - setting up meeting title and state');

      const randomTitle = generateMeetingTitle();
      setMeetingTitle(randomTitle);

      // Set STARTING status before initiating backend recording
      setStatus(RecordingStatus.STARTING, 'Initializing recording...');

      // Start the actual backend recording
      console.log('Starting backend recording with meeting:', randomTitle);
      await recordingService.startRecordingWithDevices(
        selectedDevices?.micDevice || null,
        selectedDevices?.systemDevice || null,
        randomTitle
      );
      console.log('Backend recording started successfully');

      // Update state after successful backend start
      // Note: RECORDING status will be set by RecordingStateContext event listener
      console.log('Setting isRecordingState to true');
      setIsRecording(true); // This will also update the sidebar via the useEffect
      clearTranscripts(); // Clear previous transcripts when starting new recording
      setIsMeetingActive(true);

      // Show recording notification if enabled
      await showRecordingNotification();
    } catch (error) {
      console.error('Failed to start recording:', error);
      setStatus(RecordingStatus.ERROR, error instanceof Error ? error.message : 'Failed to start recording');
      setIsRecording(false); // Reset state on error
      // Re-throw so RecordingControls can handle device-specific errors
      throw error;
    } finally {
      // Every exit path — including the early return above and the re-throw —
      // has to land here, or the record button stays disabled forever.
      endStart();
    }
  }, [generateMeetingTitle, setMeetingTitle, setIsRecording, clearTranscripts, setIsMeetingActive, checkParakeetReady, reportModelNotReady, selectedDevices, setStatus, beginStart, endStart]);

  // Check for autoStartRecording flag and start recording automatically
  useEffect(() => {
    const checkAutoStartRecording = async () => {
      if (typeof window === 'undefined') return;

      const shouldAutoStart = sessionStorage.getItem('autoStartRecording');
      if (shouldAutoStart !== 'true' || isRecording) return;

      console.log('Auto-starting recording from navigation...');
      if (!beginStart()) return;

      try {
        // Clear the flag synchronously, before the first await, so a re-render
        // cannot re-enter this effect and queue a second start.
        sessionStorage.removeItem('autoStartRecording');

        // Check if Parakeet transcription model is ready before starting
        const parakeetReady = await checkParakeetReady();
        if (!parakeetReady) {
          await reportModelNotReady();
          return;
        }

        // Generate meeting title
        const generatedMeetingTitle = generateMeetingTitle();

        // Set STARTING status before initiating backend recording
        setStatus(RecordingStatus.STARTING, 'Initializing recording...');

        console.log('Auto-starting backend recording with meeting:', generatedMeetingTitle);
        const result = await recordingService.startRecordingWithDevices(
          selectedDevices?.micDevice || null,
          selectedDevices?.systemDevice || null,
          generatedMeetingTitle
        );
        console.log('Auto-start backend recording result:', result);

        // Update UI state after successful backend start
        // Note: RECORDING status will be set by RecordingStateContext event listener
        setMeetingTitle(generatedMeetingTitle);
        setIsRecording(true);
        clearTranscripts();
        setIsMeetingActive(true);

        // Show recording notification if enabled
        await showRecordingNotification();
      } catch (error) {
        console.error('Failed to auto-start recording:', error);
        setStatus(RecordingStatus.ERROR, error instanceof Error ? error.message : 'Failed to auto-start recording');
        alert('Failed to start recording. Check console for details.');
      } finally {
        endStart();
      }
    };

    checkAutoStartRecording();
  }, [
    isRecording,
    selectedDevices,
    generateMeetingTitle,
    setMeetingTitle,
    setIsRecording,
    clearTranscripts,
    setIsMeetingActive,
    checkParakeetReady,
    reportModelNotReady,
    setStatus,
    beginStart,
    endStart,
  ]);

  // Listen for direct recording trigger from sidebar when already on home page.
  // The tray also lands here, forwarded as this window event by layout.tsx.
  useEffect(() => {
    const handleDirectStart = async () => {
      if (isRecording) {
        console.log('Recording already in progress, ignoring direct start event');
        return;
      }

      console.log('Direct start from sidebar - checking Parakeet model status');
      if (!beginStart()) {
        console.log('Start already in flight, ignoring direct start event');
        return;
      }

      try {
        // Check if Parakeet transcription model is ready before starting
        const parakeetReady = await checkParakeetReady();
        if (!parakeetReady) {
          await reportModelNotReady();
          return;
        }

        // Generate meeting title
        const generatedMeetingTitle = generateMeetingTitle();

        // Set STARTING status before initiating backend recording
        setStatus(RecordingStatus.STARTING, 'Initializing recording...');

        console.log('Starting backend recording with meeting:', generatedMeetingTitle);
        const result = await recordingService.startRecordingWithDevices(
          selectedDevices?.micDevice || null,
          selectedDevices?.systemDevice || null,
          generatedMeetingTitle
        );
        console.log('Backend recording result:', result);

        // Update UI state after successful backend start
        // Note: RECORDING status will be set by RecordingStateContext event listener
        setMeetingTitle(generatedMeetingTitle);
        setIsRecording(true);
        clearTranscripts();
        setIsMeetingActive(true);

        // Show recording notification if enabled
        await showRecordingNotification();
      } catch (error) {
        console.error('Failed to start recording from sidebar:', error);
        setStatus(RecordingStatus.ERROR, error instanceof Error ? error.message : 'Failed to start recording from sidebar');
        alert('Failed to start recording. Check console for details.');
      } finally {
        endStart();
      }
    };

    window.addEventListener('start-recording-from-sidebar', handleDirectStart);

    return () => {
      window.removeEventListener('start-recording-from-sidebar', handleDirectStart);
    };
  }, [
    isRecording,
    selectedDevices,
    generateMeetingTitle,
    setMeetingTitle,
    setIsRecording,
    clearTranscripts,
    setIsMeetingActive,
    checkParakeetReady,
    reportModelNotReady,
    setStatus,
    beginStart,
    endStart,
  ]);

  return {
    handleRecordingStart,
    isStarting,
  };
}
