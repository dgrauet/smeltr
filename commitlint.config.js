// commitlint config — enforces Conventional Commits.
// See https://github.com/conventional-changelog/commitlint
module.exports = {
  extends: ["@commitlint/config-conventional"],
  rules: {
    "header-max-length": [2, "always", 100],
  },
};
