# Publishing SlopOS to npm

## Prerequisites

1. **npm account**: Create one at https://www.npmjs.com/signup
2. **npm CLI**: Should be installed with Node.js
3. **Authentication**: Log in to npm

```bash
npm login
```

## Package Name

The package is configured as `@slopos/kernel` (scoped package). This ensures:
- Unique namespace on npm
- Clear organization
- Room for future packages like `@slopos/userland`, `@slopos/tools`, etc.

If you don't have access to the `@slopos` scope, you can either:
1. Request access at npmjs.com after publishing once
2. Change to an unscoped name in package.json: `"name": "slopos-kernel"`

## Pre-publish Checklist

1. **Verify package contents**:
```bash
npm pack --dry-run
```

This shows what files will be included in the package.

2. **Test local installation**:
```bash
npm pack
npm install -g ./slopos-kernel-0.1.0.tgz
slopos help
npm uninstall -g @slopos/kernel
```

3. **Check package.json**:
- Version number is correct
- License is accurate (GPL-3.0-only)
- Repository URL is correct
- Author/contributors are listed

## Publishing

### First-time publish (scoped package)

```bash
npm publish --access public
```

The `--access public` flag is required for scoped packages that should be publicly available.

### Subsequent publishes

1. **Update version** in package.json (following semver):
   - Patch: `0.1.1` (bug fixes)
   - Minor: `0.2.0` (new features, backward compatible)
   - Major: `1.0.0` (breaking changes)

   Or use npm version:
   ```bash
   npm version patch  # 0.1.0 -> 0.1.1
   npm version minor  # 0.1.0 -> 0.2.0
   npm version major  # 0.1.0 -> 1.0.0
   ```

2. **Publish**:
   ```bash
   npm publish
   ```

## Installation (for users)

Once published, users can install and run SlopOS with:

### Global Installation

```bash
# Install the package globally
npm install -g @slopos/kernel

# Complete setup + build in one command
slopos install

# Boot with graphical display
slopos boot-video
```

### Using npx (no install)

```bash
# One-liner to build and boot
npx @slopos/kernel install && npx @slopos/kernel boot-video
```

### Step-by-step

```bash
# 1. Setup Rust toolchain
slopos setup

# 2. Build kernel and ISO
slopos build

# 3. Boot it
slopos boot-video
```

The built ISO will be available at `builddir/slop.iso` (or wherever the user ran the command).

## Package Size Considerations

The full kernel source code will be included (~3-4 MB). Consider:
- Build artifacts are excluded via `.npmignore`
- Third-party binaries (Limine, OVMF) are excluded
- Users will need to run `slopos setup` after install

## Automation

You can automate publishing with GitHub Actions:

```yaml
name: Publish to npm

on:
  release:
    types: [created]

jobs:
  publish:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - uses: actions/setup-node@v3
        with:
          node-version: '18'
          registry-url: 'https://registry.npmjs.org'
      - run: npm publish --access public
        env:
          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
```

## Unpublishing

If you need to unpublish (within 72 hours):

```bash
npm unpublish @slopos/kernel@0.1.0
```

**WARNING**: Unpublishing is permanent and can break dependent projects. Only do this for critical issues.

## Best Practices

1. **Never publish credentials or secrets**
2. **Test the package locally before publishing**
3. **Use semantic versioning**
4. **Keep the package.json version up to date**
5. **Document breaking changes in README**
6. **Add a CHANGELOG.md** to track version history

## Verification

After publishing, verify at:
- https://www.npmjs.com/package/@slopos/kernel
- Install on a clean system to test

## Common Issues

**Issue**: `403 Forbidden - You do not have permission to publish`
**Solution**: Run `npm login` and ensure you're logged in with the correct account

**Issue**: `402 Payment Required - You must sign up for private packages`
**Solution**: Add `--access public` to your publish command

**Issue**: Package name already taken
**Solution**: Choose a different name or use a scoped package (@username/package-name)

---

**The Wheel of Fate favors the prepared. May your publish be blessed.**
