export default {
  extends: ['@commitlint/config-conventional'],
  rules: {
    // Allow PascalCase identifiers in subjects (Rust type names like
    // SessionRouter, AttachScopedProbes, etc.).
    'subject-case': [0],
  },
};
