export type SpeakerNames = Record<string, string>;

export function speakerLabel(speaker: string, names?: SpeakerNames): string {
  const named = names?.[speaker];
  if (named) return named;
  return speaker === 'you' ? 'You' : `Speaker ${speaker}`;
}

export function withSpeaker(
  text: string,
  speaker?: string,
  names?: SpeakerNames
): string {
  return speaker ? `${speakerLabel(speaker, names)}: ${text}` : text;
}
