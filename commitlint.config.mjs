export default {
  extends: ['@commitlint/config-conventional'],
  rules: {
    // Allow PascalCase identifiers in subjects (Rust type names like
    // SessionRouter, AttachScopedProbes, etc.).
    'subject-case': [0],
    // Dependabot's grouped PRs put long markdown tables + URLs in the commit
    // body and footer; don't reject those on line length.
    'body-max-line-length': [0],
    'footer-max-line-length': [0],
  },
};
