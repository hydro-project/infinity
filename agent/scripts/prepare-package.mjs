#!/usr/bin/env node
// Prepares this package for consumption as an npm dependency.
//
// Runs as the `prepare` lifecycle script, so it executes both on plain
// `npm install` in this directory (in-repo development) and when a package
// manager builds the package from a git dependency (e.g.
// `pnpm add "github:hydro-project/infinity#path:agent"`).
//
// It does two things:
//   1. Compiles the construct library (lib/infinity-agents + the lib/index.ts
//      barrel) to JavaScript + type declarations, in place.
//   2. Vendors the Rust workspace (crates/, Cargo.toml, Cargo.lock, clippy.toml)
//      from the repository root into ./rust, so the `InfinityAgent` construct
//      can build the agent Lambda even when the package is installed outside
//      the Infinity repository.
import { execFileSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, rmSync, copyFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const packageDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const repoRoot = path.dirname(packageDir);
const require = createRequire(import.meta.url);

// 1. Compile the construct library.
const tsc = require.resolve('typescript/bin/tsc');
execFileSync(process.execPath, [tsc, '-p', path.join(packageDir, 'tsconfig.lib.json')], {
  stdio: 'inherit',
});
console.log('prepare-package: compiled lib/infinity-agents');

// 2. Vendor the Rust workspace, when building from inside the repository.
const cratesDir = path.join(repoRoot, 'crates');
if (existsSync(path.join(cratesDir, 'infinity-agent-lambda', 'Cargo.toml'))) {
  const rustDir = path.join(packageDir, 'rust');
  rmSync(rustDir, { recursive: true, force: true });
  mkdirSync(rustDir, { recursive: true });
  cpSync(cratesDir, path.join(rustDir, 'crates'), {
    recursive: true,
    filter: (src) => {
      const base = path.basename(src);
      return base !== 'target' && base !== 'node_modules';
    },
  });
  for (const file of ['Cargo.toml', 'Cargo.lock', 'clippy.toml']) {
    copyFileSync(path.join(repoRoot, file), path.join(rustDir, file));
  }
  console.log('prepare-package: vendored Rust workspace into rust/');
} else if (existsSync(path.join(packageDir, 'rust', 'crates', 'infinity-agent-lambda', 'Cargo.toml'))) {
  console.log('prepare-package: using existing vendored Rust workspace in rust/');
} else {
  console.warn(
    'prepare-package: warning: no Rust workspace found (neither ../crates nor ./rust); ' +
      'the InfinityAgent construct will require an explicit `codePath`.'
  );
}
