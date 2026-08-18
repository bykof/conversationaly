import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { SpeakerNames } from '@/lib/speaker';

export function useSpeakerNames(meetingId?: string) {
  const [speakerNames, setSpeakerNames] = useState<SpeakerNames>({});

  useEffect(() => {
    if (!meetingId) {
      setSpeakerNames({});
      return;
    }
    let live = true;
    invoke<SpeakerNames>('get_speaker_names', { meetingId })
      .then((names) => {
        if (live) setSpeakerNames(names);
      })
      .catch((e) => console.warn('Failed to load speaker names:', e));
    return () => {
      live = false;
    };
  }, [meetingId]);

  const renameSpeaker = useCallback(
    (speaker: string, name: string) => {
      if (!meetingId) return;
      const trimmed = name.trim();
      setSpeakerNames((prev) => {
        const next = { ...prev };
        if (trimmed) next[speaker] = trimmed;
        else delete next[speaker];
        return next;
      });
      invoke('set_speaker_name', { meetingId, speaker, name: trimmed }).catch((e) =>
        console.warn('Failed to save speaker name:', e)
      );
    },
    [meetingId]
  );

  return { speakerNames, renameSpeaker };
}
