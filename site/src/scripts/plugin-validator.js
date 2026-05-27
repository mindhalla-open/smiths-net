/**
 * Plugin Validator — client-side JSON schema checks for
 * Smiths-Net plugin envelopes.
 *
 * Validates:
 *   - describe() output (capability descriptor)
 *   - invoke()  response (result or error envelope)
 *   - plugin.toml manifest (key fields)
 */

/**
 * @typedef {{ ok: boolean, checks: Array<{ label: string, pass: boolean, detail?: string }> }} ValidationResult
 */

/**
 * Validate a describe() JSON output.
 * @param {string} jsonString
 * @returns {ValidationResult}
 */
export function validateDescribe(jsonString) {
  const checks = [];

  // Parse JSON
  let data;
  try {
    data = JSON.parse(jsonString);
  } catch (e) {
    checks.push({ label: 'Valid JSON', pass: false, detail: e.message });
    return { ok: false, checks };
  }
  checks.push({ label: 'Valid JSON', pass: true });

  // Normalize to array
  const descriptors = Array.isArray(data) ? data : [data];

  if (descriptors.length === 0) {
    checks.push({ label: 'At least one descriptor', pass: false, detail: 'Empty array' });
    return { ok: false, checks };
  }
  checks.push({ label: 'At least one descriptor', pass: true, detail: `${descriptors.length} descriptor(s)` });

  // Validate each descriptor
  for (let i = 0; i < descriptors.length; i++) {
    const d = descriptors[i];
    const prefix = descriptors.length > 1 ? `[${i}] ` : '';

    // Required fields
    const hasCapability = typeof d.capability === 'string' && d.capability.length > 0;
    checks.push({
      label: `${prefix}Has "capability" field`,
      pass: hasCapability,
      detail: hasCapability ? d.capability : 'missing or empty',
    });

    const hasPlugin = typeof d.plugin === 'string' && d.plugin.length > 0;
    checks.push({
      label: `${prefix}Has "plugin" field`,
      pass: hasPlugin,
      detail: hasPlugin ? d.plugin : 'missing or empty',
    });

    const hasAbi = typeof d.abi === 'string' && d.abi.length > 0;
    checks.push({
      label: `${prefix}Has "abi" field`,
      pass: hasAbi,
      detail: hasAbi ? d.abi : 'missing or empty',
    });

    // Optional but recommended
    if (d.description !== undefined) {
      const hasDesc = typeof d.description === 'string' && d.description.length > 0;
      checks.push({
        label: `${prefix}Has "description"`,
        pass: hasDesc,
        detail: hasDesc ? `${d.description.slice(0, 60)}…` : 'empty',
      });
    }
  }

  const ok = checks.every((c) => c.pass);
  return { ok, checks };
}

/**
 * Validate an invoke() JSON response.
 * @param {string} jsonString
 * @returns {ValidationResult}
 */
export function validateInvoke(jsonString) {
  const checks = [];

  let data;
  try {
    data = JSON.parse(jsonString);
  } catch (e) {
    checks.push({ label: 'Valid JSON', pass: false, detail: e.message });
    return { ok: false, checks };
  }
  checks.push({ label: 'Valid JSON', pass: true });

  // Must be an object
  if (typeof data !== 'object' || data === null || Array.isArray(data)) {
    checks.push({ label: 'Is an object', pass: false, detail: `Got ${typeof data}` });
    return { ok: false, checks };
  }
  checks.push({ label: 'Is an object', pass: true });

  // Must have "result" or "error" — not both
  const hasResult = 'result' in data;
  const hasError = 'error' in data;

  if (!hasResult && !hasError) {
    checks.push({
      label: 'Has "result" or "error"',
      pass: false,
      detail: 'Neither "result" nor "error" key found',
    });
    return { ok: false, checks };
  }

  if (hasResult && hasError) {
    checks.push({
      label: 'Exclusive "result"/"error"',
      pass: false,
      detail: 'Both "result" and "error" present — pick one',
    });
  } else {
    checks.push({ label: 'Has "result" or "error"', pass: true, detail: hasResult ? 'result' : 'error' });
  }

  // Validate error envelope
  if (hasError) {
    const err = data.error;
    const isObj = typeof err === 'object' && err !== null && !Array.isArray(err);
    checks.push({ label: 'Error is an object', pass: isObj });

    if (isObj) {
      const hasCode = typeof err.code === 'number';
      checks.push({
        label: 'Error has numeric "code"',
        pass: hasCode,
        detail: hasCode ? `code=${err.code}` : 'missing or non-numeric',
      });

      const hasMsg = typeof err.message === 'string';
      checks.push({
        label: 'Error has string "message"',
        pass: hasMsg,
        detail: hasMsg ? err.message : 'missing or non-string',
      });
    }
  }

  const ok = checks.every((c) => c.pass);
  return { ok, checks };
}

/**
 * Validate a plugin.toml manifest.
 * @param {string} tomlString
 * @returns {ValidationResult}
 */
export function validateManifest(tomlString) {
  const checks = [];

  // Simple TOML key=value parser (no nested tables needed for plugin.toml).
  const fields = {};
  for (const line of tomlString.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    const eq = trimmed.indexOf('=');
    if (eq < 0) continue;
    const key = trimmed.slice(0, eq).trim();
    let val = trimmed.slice(eq + 1).trim();
    // Strip inline comments
    const commentIdx = val.indexOf('#');
    if (commentIdx > 0) val = val.slice(0, commentIdx).trim();
    // Strip quotes
    if ((val.startsWith('"') && val.endsWith('"')) || (val.startsWith("'") && val.endsWith("'"))) {
      val = val.slice(1, -1);
    }
    fields[key] = val;
  }

  checks.push({ label: 'Parseable TOML', pass: Object.keys(fields).length > 0 });

  // Required fields
  for (const req of ['name', 'version', 'type', 'entry', 'abi']) {
    const val = fields[req];
    const has = typeof val === 'string' && val.length > 0;
    checks.push({
      label: `Has "${req}"`,
      pass: has,
      detail: has ? val : 'missing',
    });
  }

  // Type must be one of known values
  const validTypes = ['wasm', 'script', 'sidecar'];
  if (fields.type) {
    const validType = validTypes.includes(fields.type);
    checks.push({
      label: `Type is valid (${validTypes.join('|')})`,
      pass: validType,
      detail: fields.type,
    });
  }

  // Entry extension matches type
  if (fields.type && fields.entry) {
    const ext = fields.entry.split('.').pop();
    const expectedExts = { wasm: 'wasm', script: 'rhai', sidecar: null };
    const expected = expectedExts[fields.type];
    if (expected) {
      const match = ext === expected;
      checks.push({
        label: `Entry extension matches type`,
        pass: match,
        detail: match ? `.${ext}` : `expected .${expected}, got .${ext}`,
      });
    }
  }

  const ok = checks.every((c) => c.pass);
  return { ok, checks };
}
