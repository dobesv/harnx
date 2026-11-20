import { describe, expect, it } from 'vitest';
import cases from '../../../crates/harnx-core/tests/fixtures/a2ui-build-data-cases.json';
import {
  actionIsDeclared,
  buildData,
  defaultValue,
  findAction,
  inputFields,
  validateComponentGraph,
  MAX_A2UI_DEPTH,
  MAX_A2UI_EXPANSION,
} from '../a2ui/buildData';

describe('a2ui buildData parity with Rust', () => {
  for (const caseItem of cases) {
    it(`matches case: ${caseItem.name}`, () => {
      const actual = buildData(caseItem.surface, caseItem.action, caseItem.values);
      expect(actual).toEqual(caseItem.expected);
    });
  }

  it('throws undeclared action error matching Rust message', () => {
    const surface = cases[0].surface;
    expect(() => buildData(surface, 'publish', {})).toThrow(
      "action 'publish' is not declared by this surface (and is not 'deny')"
    );
  });

  it('exposes input fields and button actions', () => {
    const surface = cases[0].surface;
    const fields = inputFields(surface);
    expect(fields).toHaveLength(2);
    expect(fields[0].key).toBe('choice');
    expect(fields[0].kind).toBe('ChoicePicker');
    expect(fields[0].exclusive).toBe(true);
    expect(fields[1].key).toBe('name');
    expect(fields[1].kind).toBe('TextField');

    const action = findAction(surface, 'deploy');
    expect(action).toBeDefined();
    expect(action?.name).toBe('deploy');
    expect(action?.label).toBe('Deploy');

    expect(actionIsDeclared(surface, 'deploy')).toBe(true);
    expect(actionIsDeclared(surface, 'nonexistent')).toBe(false);
  });

  it('returns default values for untouched fields', () => {
    const surface = cases[0].surface;
    expect(defaultValue(surface, '/choice', true)).toEqual([]);
    expect(defaultValue(surface, '/name', false)).toBe('');
    expect(defaultValue(surface, '/region', false)).toBe('eu');
  });

  describe('validateComponentGraph', () => {
    it('returns valid: true for acyclic graphs', () => {
      const map = new Map<string, any>([
        ['root', { id: 'root', component: 'Column', children: ['c1', 'c2'] }],
        ['c1', { id: 'c1', component: 'Card', child: 't1' }],
        ['c2', { id: 'c2', component: 'Text', text: 'Hello' }],
        ['t1', { id: 't1', component: 'Text', text: 'World' }],
      ]);
      expect(validateComponentGraph('root', map)).toEqual({ valid: true });
    });

    it('detects self-referencing cycle', () => {
      const map = new Map<string, any>([
        ['root', { id: 'root', component: 'Card', child: 'root' }],
      ]);
      const res = validateComponentGraph('root', map);
      expect(res.valid).toBe(false);
      expect(res.error).toBe('Cyclic reference detected at component "root"');
    });

    it('detects two-node cycle', () => {
      const map = new Map<string, any>([
        ['node-a', { id: 'node-a', component: 'Card', child: 'node-b' }],
        ['node-b', { id: 'node-b', component: 'Card', child: 'node-a' }],
      ]);
      const res = validateComponentGraph('node-a', map);
      expect(res.valid).toBe(false);
      expect(res.error).toBe('Cyclic reference detected at component "node-a"');
    });

    it('detects multi-node cycle (A -> B -> C -> A)', () => {
      const map = new Map<string, any>([
        ['a', { id: 'a', component: 'Column', children: ['b'] }],
        ['b', { id: 'b', component: 'Card', child: 'c' }],
        ['c', { id: 'c', component: 'Column', children: ['a'] }],
      ]);
      const res = validateComponentGraph('a', map);
      expect(res.valid).toBe(false);
      expect(res.error).toBe('Cyclic reference detected at component "a"');
    });

    it('detects depth exceeding MAX_A2UI_DEPTH', () => {
      const map = new Map<string, any>();
      for (let i = 0; i <= MAX_A2UI_DEPTH + 2; i++) {
        map.set(`d-${i}`, { id: `d-${i}`, component: 'Card', child: `d-${i + 1}` });
      }
      map.set(`d-${MAX_A2UI_DEPTH + 3}`, { id: `d-${MAX_A2UI_DEPTH + 3}`, component: 'Text', text: 'End' });

      const res = validateComponentGraph('d-0', map);
      expect(res.valid).toBe(false);
      expect(res.error).toBe(`Component hierarchy exceeds maximum depth of ${MAX_A2UI_DEPTH}`);
    });

    it('accepts deep component graph at MAX_A2UI_DEPTH (64)', () => {
      const map = new Map<string, any>();
      for (let i = 0; i < MAX_A2UI_DEPTH - 1; i++) {
        map.set(`d-${i}`, { id: `d-${i}`, component: 'Card', child: `d-${i + 1}` });
      }
      map.set(`d-${MAX_A2UI_DEPTH - 1}`, { id: `d-${MAX_A2UI_DEPTH - 1}`, component: 'Text', text: 'End' });

      const res = validateComponentGraph('d-0', map);
      expect(res).toEqual({ valid: true });
    });

    it('detects expansion exceeding MAX_A2UI_EXPANSION (exponential DAG)', () => {
      const map = new Map<string, any>();
      // 13 layers of doubling: 2^13 = 8192 > 4096
      for (let i = 0; i < 13; i++) {
        map.set(`dag-${i}`, {
          id: `dag-${i}`,
          component: 'Column',
          children: [`dag-${i + 1}`, `dag-${i + 1}`],
        });
      }
      map.set('dag-13', { id: 'dag-13', component: 'Text', text: 'Leaf' });

      const res = validateComponentGraph('dag-0', map);
      expect(res.valid).toBe(false);
      expect(res.error).toBe(`Component hierarchy exceeds maximum expansion of ${MAX_A2UI_EXPANSION}`);
    });

    it('accepts shared DAG within expansion limit', () => {
      const map = new Map<string, any>();
      // 5 layers of doubling: 2^5 = 32 << 4096
      for (let i = 0; i < 5; i++) {
        map.set(`dag-${i}`, {
          id: `dag-${i}`,
          component: 'Column',
          children: [`dag-${i + 1}`, `dag-${i + 1}`],
        });
      }
      map.set('dag-5', { id: 'dag-5', component: 'Text', text: 'Leaf' });

      const res = validateComponentGraph('dag-0', map);
      expect(res).toEqual({ valid: true });
    });
  });
});
