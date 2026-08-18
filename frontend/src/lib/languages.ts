/**
 * Language codes -> names, via the platform.
 *
 * The catalog stores each model's own `general.languages` GGUF metadata, which
 * is codes: `de`, or BCP-47 locales like `de-DE` where the model distinguishes
 * them. `Intl.DisplayNames` turns those into names in the reader's own locale,
 * so there is no table here to fall out of date with the catalog.
 */

let displayNames: Intl.DisplayNames | null = null;
let regionNames: Intl.DisplayNames | null = null;

function names(): Intl.DisplayNames | null {
  if (displayNames === null) {
    try {
      displayNames = new Intl.DisplayNames(undefined, {
        type: 'language',
        fallback: 'none',
      });
      regionNames = new Intl.DisplayNames(undefined, {
        type: 'region',
        fallback: 'none',
      });
    } catch {
      return null;
    }
  }
  return displayNames;
}

/** `de-DE` -> "German (Germany)". Falls back to the raw code, never to a guess. */
export function languageName(code: string): string {
  const intl = names();
  if (!intl) return code;

  const [base, region] = code.split('-');
  const language = intl.of(base);
  if (!language) return code;

  const place = region && regionNames ? regionNames.of(region.toUpperCase()) : undefined;
  return place ? `${language} (${place})` : language;
}

/** Every name and code for one model, deduped, alphabetical. */
export function languageNames(codes: string[]): string[] {
  return [...new Set(codes.map(languageName))].sort((a, b) => a.localeCompare(b));
}

/** The short label on a model row. */
export function languagesSummary(codes: string[]): string {
  if (codes.length === 0) return 'Unknown';
  if (codes.length === 1) return `${languageName(codes[0])} only`;
  return `${codes.length} languages`;
}

/**
 * The haystack a language search matches against: names and raw codes, so both
 * "German" and "de" find the same models.
 */
export function languageHaystack(codes: string[]): string {
  return [...codes, ...codes.map(languageName)].join(' ').toLowerCase();
}
