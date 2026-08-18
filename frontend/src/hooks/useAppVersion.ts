import { useEffect, useState } from 'react';
import { getVersion } from '@tauri-apps/api/app';

/**
 * The app version, read from `src-tauri/tauri.conf.json` at build time — the
 * one place it is declared, and the same file the release workflow tags from.
 *
 * Empty until it resolves rather than a hardcoded placeholder: the sidebar
 * shipped `const APP_VERSION = '1.0.0'` for three releases because a plausible
 * fallback never looks wrong enough to notice.
 */
export function useAppVersion(): string {
  const [version, setVersion] = useState('');

  useEffect(() => {
    getVersion().then(setVersion).catch(console.error);
  }, []);

  return version;
}
