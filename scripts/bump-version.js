const fs = require('fs');
const path = require('path');

const cargoTomlPath = path.join(__dirname, '..', 'Cargo.toml');
const content = fs.readFileSync(cargoTomlPath, 'utf-8');

// Extract current version
const versionMatch = content.match(/^version = "(\d+)\.(\d+)\.(\d+)"/m);
if (!versionMatch) {
  console.error('Could not find version in Cargo.toml');
  process.exit(1);
}

let [, major, minor, patch] = versionMatch;
major = parseInt(major);
minor = parseInt(minor);
patch = parseInt(patch);

const bumpType = process.argv[2]; // 'minor' or 'patch'

if (bumpType === 'minor') {
  minor += 1;
  patch = 0;
} else if (bumpType === 'patch') {
  patch += 1;
} else {
  console.error('Invalid bump type. Use "minor" or "patch"');
  process.exit(1);
}

const newVersion = `${major}.${minor}.${patch}`;

// Update Cargo.toml
const updatedContent = content.replace(
  /^version = "\d+\.\d+\.\d+"/m,
  `version = "${newVersion}"`
);
fs.writeFileSync(cargoTomlPath, updatedContent);

console.log(newVersion);
