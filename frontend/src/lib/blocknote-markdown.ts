import type { Block } from "@blocknote/core";

interface MarkdownCapableEditor {
  // Sync since BlockNote 0.53, a Promise before it. `await` below covers both.
  blocksToMarkdownLossy: (blocks: Block[]) => string | Promise<string>;
}

interface MarkdownConversionOptions {
  source: string;
  fallbackMarkdown?: string;
}

interface MarkdownConversionResult {
  markdown?: string;
  ok: boolean;
}

export async function blocksToMarkdownSafely(
  editor: MarkdownCapableEditor,
  blocks: Block[],
  options: MarkdownConversionOptions,
): Promise<MarkdownConversionResult> {
  try {
    return {
      markdown: await editor.blocksToMarkdownLossy(blocks),
      ok: true,
    };
  } catch (error) {
    console.error("Failed to convert BlockNote blocks to markdown", {
      source: options.source,
      blocksCount: blocks.length,
      error,
    });

    return {
      markdown: options.fallbackMarkdown,
      ok: false,
    };
  }
}
