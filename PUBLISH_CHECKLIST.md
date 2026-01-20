# Publishing Checklist for SlopOS npm Package

Before publishing to npm, go through this checklist:

## Pre-Publish Checklist

### 1. Version and Metadata
- [ ] Update version in `package.json` (use semantic versioning)
- [ ] Verify author/contributors are correct
- [ ] Check repository URL is accurate
- [ ] Confirm license is GPL-3.0-only

### 2. Test Package Locally

```bash
# Preview what will be published
npm pack --dry-run

# Create actual tarball
npm pack

# Test installation locally
npm install -g ./slopos-kernel-0.1.0.tgz

# Test commands
slopos help
slopos setup
# (optional) slopos build
# (optional) slopos boot-video

# Uninstall test
npm uninstall -g @slopos/kernel
```

### 3. Verify Package Contents

- [ ] `bin/slopos.js` is included and executable
- [ ] `bin/postinstall.js` is included
- [ ] All source directories included (boot/, mm/, drivers/, etc.)
- [ ] Build files included (Makefile, link.ld, etc.)
- [ ] Documentation included (README.md, lore/, etc.)
- [ ] Build artifacts excluded (builddir/, *.iso, etc.)
- [ ] node_modules excluded

### 4. Test Scripts

```bash
# Test that npm scripts work
npm run setup
npm run build
npm run boot  # (if time permits)
```

### 5. Documentation

- [ ] README.md is up to date
- [ ] NPM_QUICKSTART.md explains installation
- [ ] NPM_PUBLISHING.md has publishing instructions
- [ ] CLAUDE.md is current
- [ ] Lore files are included

### 6. Authentication

```bash
# Login to npm
npm login

# Verify you're logged in
npm whoami
```

### 7. Publish

```bash
# For first-time publish (scoped package)
npm publish --access public

# For subsequent publishes
npm publish

# For beta/test releases
npm publish --tag beta
```

### 8. Post-Publish Verification

- [ ] Check package page: https://www.npmjs.com/package/@slopos/kernel
- [ ] Test fresh installation on clean system
- [ ] Verify README displays correctly on npm
- [ ] Test npx command works

```bash
# Test as a user would
npx @slopos/kernel help
```

### 9. Announce

- [ ] Update GitHub README with npm installation instructions
- [ ] Tag the release in git: `git tag v0.1.0 && git push --tags`
- [ ] Create GitHub release
- [ ] Update CHANGELOG (if exists)

## Version Bump Commands

```bash
# Patch release (0.1.0 -> 0.1.1) - bug fixes
npm version patch

# Minor release (0.1.0 -> 0.2.0) - new features
npm version minor

# Major release (0.1.0 -> 1.0.0) - breaking changes
npm version major

# Then publish
npm publish
```

## Rollback (if needed)

```bash
# Unpublish specific version (within 72 hours)
npm unpublish @slopos/kernel@0.1.0

# Deprecate a version (preferred over unpublish)
npm deprecate @slopos/kernel@0.1.0 "This version has issues, use 0.1.1 instead"
```

## Common Issues

**403 Forbidden**
- Solution: Run `npm login` and ensure correct account

**Package name taken**
- Solution: Use scoped package `@username/slopos-kernel`

**Files missing from package**
- Solution: Check `.npmignore` and `files` in package.json

**Postinstall script fails**
- Solution: Test with `node bin/postinstall.js` locally first

## Security Notes

- [ ] No secrets or credentials in source
- [ ] No API keys in code
- [ ] `.env` files excluded
- [ ] No personal data included

## Final Check

```bash
# One final dry run
npm publish --dry-run

# If all looks good:
npm publish --access public
```

---

**The Wheel of Fate favors the prepared. Publish wisely.** 🎰
