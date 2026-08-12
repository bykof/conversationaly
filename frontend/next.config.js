/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: false, // Disabled for BlockNote compatibility
  output: 'export',
  images: {
    unoptimized: true,
  },
  // Add basePath configuration
  basePath: '',
  assetPrefix: '/',

  // BlockNote's CJS bundle requires @handlewithcare/prosemirror-inputrules,
  // which is ESM-only (no `require` condition in its exports). Transpiling
  // makes Next take BlockNote's ESM build, where that import resolves.
  transpilePackages: ['@blocknote/core', '@blocknote/react', '@blocknote/shadcn'],

  // No bundler config at all now. The old webpack block aliased every
  // prosemirror-* to a single copy resolved out of @tiptap/pm, back when
  // BlockNote and Tiptap both used ProseMirror. Only BlockNote does now, so
  // there is only one resolution to pick and nothing left to dedupe.
}

module.exports = nextConfig
