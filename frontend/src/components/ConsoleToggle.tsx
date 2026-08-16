import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Button } from '@/components/ui/button';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';

export function ConsoleToggle() {
  const [isLoading, setIsLoading] = useState(false);
  const [consoleVisible, setConsoleVisible] = useState(false);
  const [logPath, setLogPath] = useState<string | null>(null);

  // The rotating log file the Rust side writes (tauri-plugin-log). Deliberately
  // our own `get_log_file_path` command and not the plugin's JS binding: the
  // plugin has no capability entry in tauri.conf.json, and it does not need one
  // as long as nothing in the webview calls it.
  useEffect(() => {
    invoke<string>('get_log_file_path')
      .then(setLogPath)
      .catch((error) => console.error('Failed to resolve log file path:', error));
  }, []);

  const handleRevealLogFile = async () => {
    setIsLoading(true);
    try {
      await invoke('reveal_log_file');
    } catch (error) {
      console.error('Failed to reveal log file:', error);
    } finally {
      setIsLoading(false);
    }
  };

  const handleToggleConsole = async () => {
    setIsLoading(true);
    try {
      const result = await invoke('toggle_console');
      console.log('Console toggle result:', result);
      setConsoleVisible(!consoleVisible);
    } catch (error) {
      console.error('Failed to toggle console:', error);
    } finally {
      setIsLoading(false);
    }
  };

  const handleShowConsole = async () => {
    setIsLoading(true);
    try {
      const result = await invoke('show_console');
      console.log('Show console result:', result);
      setConsoleVisible(true);
    } catch (error) {
      console.error('Failed to show console:', error);
    } finally {
      setIsLoading(false);
    }
  };

  const handleHideConsole = async () => {
    setIsLoading(true);
    try {
      const result = await invoke('hide_console');
      console.log('Hide console result:', result);
      setConsoleVisible(false);
    } catch (error) {
      console.error('Failed to hide console:', error);
    } finally {
      setIsLoading(false);
    }
  };

  // Only show this component on Windows or macOS
  if (typeof window !== 'undefined') {
    const userAgent = window.navigator.userAgent;
    if (!userAgent.includes('Windows') && !userAgent.includes('Mac')) {
      return null;
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <Label htmlFor="console-toggle">
          Developer Console
        </Label>
        <Switch
          id="console-toggle"
          checked={consoleVisible}
          onCheckedChange={(checked) => {
            if (checked) {
              handleShowConsole();
            } else {
              handleHideConsole();
            }
          }}
          disabled={isLoading}
        />
      </div>
      <div className="flex space-x-2">
        <Button
          variant="outline"
          size="sm"
          onClick={handleToggleConsole}
          disabled={isLoading}
        >
          Toggle Console
        </Button>
        <Button
          variant="outline"
          size="sm"
          onClick={handleRevealLogFile}
          disabled={isLoading}
          title={logPath ?? undefined}
        >
          Reveal log file
        </Button>
      </div>
      <p className="text-sm text-muted-foreground">
        Show or hide the developer console window. On Windows, this controls the console window. On macOS, this opens Terminal with app logs.
      </p>
      <p className="text-sm text-muted-foreground">
        The app also writes a rotating log file you can attach to a bug report.
        {logPath ? (
          <>
            {' '}
            <span className="break-all font-mono text-xs">{logPath}</span>
          </>
        ) : null}
      </p>
    </div>
  );
}