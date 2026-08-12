module.exports = {
  plugins: {
    // Tailwind 4 ships its own PostCSS plugin and vendor-prefixes internally,
    // so autoprefixer is gone.
    '@tailwindcss/postcss': {},
  },
}
