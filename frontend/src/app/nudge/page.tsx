'use client'

import { useEffect, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { Button } from '@/components/ui/button'

declare global {
  interface Window {
    __NUDGE_HEADLINE?: string
  }
}

/**
 * The card shown when a call starts and nothing is recording.
 *
 * Rust injects the headline as `window.__NUDGE_HEADLINE` before this loads —
 * it differs by platform (microphone on macOS, app launch elsewhere) — so the
 * window needs no IPC just to know what it is talking about.
 */
export default function NudgePage() {
  const [headline, setHeadline] = useState('A call may have started')

  useEffect(() => {
    if (window.__NUDGE_HEADLINE) setHeadline(window.__NUDGE_HEADLINE)
  }, [])

  // Both buttons close the window from Rust, which owns it.
  const start = () => invoke('nudge_start_recording').catch(console.error)
  const dismiss = () => invoke('nudge_dismiss').catch(console.error)

  return (
    <>
      {/* The window is transparent so the card can have its own corners. */}
      <style>{`html, body { background: transparent !important; }`}</style>
      <div className="flex h-screen w-screen flex-col justify-between rounded-lg border border-line bg-elevated p-4 shadow-pop">
        <div>
          <p className="text-sm font-medium text-ink">{headline}</p>
          <p className="mt-0.5 text-2xs text-ink-muted">
            Conversationaly is not recording.
          </p>
        </div>

        <div className="flex items-center justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={dismiss}>
            Not now
          </Button>
          <Button variant="destructive" size="sm" onClick={start}>
            Start recording
          </Button>
        </div>
      </div>
    </>
  )
}
