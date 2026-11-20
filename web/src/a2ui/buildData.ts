import type {
  A2UIComponent,
  ButtonActionDescriptor,
  InputFieldDescriptor,
} from './types';

export function dataKey(path: string): string {
  return path.startsWith('/') ? path.slice(1) : path;
}

export function getDataModel(payload: unknown): Record<string, unknown> | undefined {
  if (!payload || typeof payload !== 'object') return undefined;
  const create = (payload as Record<string, unknown>).createSurface;
  if (!create || typeof create !== 'object') return undefined;
  const dm = (create as Record<string, unknown>).dataModel;
  if (!dm || typeof dm !== 'object' || Array.isArray(dm)) return undefined;
  return dm as Record<string, unknown>;
}

export function getComponents(payload: unknown): A2UIComponent[] {
  if (!payload || typeof payload !== 'object') return [];
  const create = (payload as Record<string, unknown>).createSurface;
  if (!create || typeof create !== 'object') return [];
  const comps = (create as Record<string, unknown>).components;
  if (!Array.isArray(comps)) return [];
  return comps as A2UIComponent[];
}

export function pointerGet(dataModel: unknown, pointer: string): unknown {
  if (!dataModel || typeof dataModel !== 'object') return undefined;
  let current: unknown = dataModel;
  const stripped = pointer.startsWith('/') ? pointer.slice(1) : pointer;
  if (stripped.length === 0) return current;

  for (const rawToken of stripped.split('/')) {
    if (rawToken.length === 0) continue;
    const token = rawToken.replace(/~1/g, '/').replace(/~0/g, '~');
    if (current === null || current === undefined || typeof current !== 'object') {
      return undefined;
    }
    if (Array.isArray(current)) {
      const idx = Number.parseInt(token, 10);
      if (Number.isNaN(idx) || idx < 0 || idx >= current.length) return undefined;
      current = current[idx];
    } else {
      const rec = current as Record<string, unknown>;
      if (!Object.prototype.hasOwnProperty.call(rec, token)) return undefined;
      current = rec[token];
    }
  }
  return current;
}

export function defaultValue(payload: unknown, path: string, isChoice: boolean): unknown {
  const dm = getDataModel(payload);
  const found = pointerGet(dm, path);
  if (found !== undefined) {
    return structuredClone(found);
  }
  return isChoice ? [] : '';
}

export function resolveDynamicString(
  payload: unknown,
  val: unknown,
  values?: Record<string, unknown>
): string | undefined {
  if (typeof val === 'string') return val;
  if (val && typeof val === 'object' && typeof (val as Record<string, unknown>)['@path'] === 'string') {
    const pointer = (val as Record<string, unknown>)['@path'] as string;
    const key = dataKey(pointer);
    if (values && values[key] !== undefined) {
      const userVal = values[key];
      return typeof userVal === 'string' ? userVal : String(userVal ?? '');
    }
    const resolved = pointerGet(getDataModel(payload), pointer);
    return typeof resolved === 'string' ? resolved : undefined;
  }
  return undefined;
}

export function inputFields(payload: unknown): InputFieldDescriptor[] {
  const comps = getComponents(payload);
  const result: InputFieldDescriptor[] = [];

  for (const comp of comps) {
    if (!comp || typeof comp !== 'object') continue;
    const id = comp.id;
    const kind = comp.component;
    if (kind !== 'TextField' && kind !== 'ChoicePicker') continue;

    const valueBinding = comp.value;
    if (!valueBinding || typeof valueBinding !== 'object' || typeof valueBinding['@path'] !== 'string') {
      continue;
    }
    const path = valueBinding['@path'];
    const key = dataKey(path);
    const label = resolveDynamicString(payload, comp.label);

    const options: Array<{ label: string; value: string }> = [];
    let exclusive = true;

    if (kind === 'ChoicePicker') {
      exclusive = comp.variant !== 'multipleSelection';
      if (Array.isArray(comp.options)) {
        for (const opt of comp.options) {
          if (!opt || typeof opt !== 'object') continue;
          if (typeof opt.value !== 'string') continue;
          const optLabel = resolveDynamicString(payload, opt.label) ?? opt.value;
          options.push({ label: optLabel, value: opt.value });
        }
      }
    }

    result.push({
      componentId: id,
      path,
      key,
      kind,
      label,
      options,
      exclusive,
    });
  }

  return result;
}

export function buttonActions(payload: unknown): ButtonActionDescriptor[] {
  const comps = getComponents(payload);
  const result: ButtonActionDescriptor[] = [];

  for (const comp of comps) {
    if (!comp || comp.component !== 'Button') continue;
    const name = comp.action?.event?.name;
    if (typeof name !== 'string' || name.length === 0) continue;

    let label: string | undefined;
    if (typeof comp.child === 'string') {
      const childComp = comps.find((c) => c.id === comp.child);
      if (childComp && childComp.component === 'Text') {
        label = resolveDynamicString(payload, childComp.text);
      }
    }

    const context = comp.action?.event?.context;
    result.push({
      componentId: comp.id,
      name,
      label,
      context: context && typeof context === 'object' && !Array.isArray(context)
        ? (context as Record<string, unknown>)
        : {},
    });
  }

  return result;
}

export function findAction(payload: unknown, action: string): ButtonActionDescriptor | undefined {
  return buttonActions(payload).find((btn) => btn.name === action);
}

export function actionIsDeclared(payload: unknown, action: string): boolean {
  return findAction(payload, action) !== undefined;
}

function resolveContextEntry(
  payload: unknown,
  entry: unknown,
  values: Record<string, unknown>
): unknown {
  if (entry && typeof entry === 'object' && !Array.isArray(entry)) {
    const rec = entry as Record<string, unknown>;
    if (typeof rec['@path'] === 'string') {
      const pointer = rec['@path'];
      const key = dataKey(pointer);
      if (values[key] !== undefined) {
        return structuredClone(values[key]);
      }
      const found = pointerGet(getDataModel(payload), pointer);
      return found !== undefined ? structuredClone(found) : null;
    }
  }
  return structuredClone(entry);
}

export function buildData(
  payload: unknown,
  action: string,
  values: Record<string, unknown>
): Record<string, unknown> {
  if (action === 'deny') {
    return {};
  }

  const button = findAction(payload, action);
  if (!button) {
    throw new Error(`action '${action}' is not declared by this surface (and is not 'deny')`);
  }

  const fields = inputFields(payload);
  const data: Record<string, unknown> = {};

  for (const field of fields) {
    if (values[field.key] !== undefined) {
      data[field.key] = structuredClone(values[field.key]);
    } else {
      data[field.key] = defaultValue(payload, field.path, field.kind === 'ChoicePicker');
    }
  }

  for (const [key, entry] of Object.entries(button.context)) {
    data[key] = resolveContextEntry(payload, entry, values);
  }

  return data;
}

// Must match MAX_COMPONENT_DEPTH and MAX_COMPONENT_EXPANSION in
// crates/harnx-core/src/interaction/surface/graph.rs: the backend rejects
// surfaces that exceed the shared budget, so anything the web renders has
// already passed the core validator. Change both sides together.
export const MAX_A2UI_DEPTH = 64;
export const MAX_A2UI_EXPANSION = 4096;

interface GraphSize {
  depth: number;
  expansion: number;
}

export function validateComponentGraph(
  rootId: string,
  componentMap: Map<string, A2UIComponent>,
  maxDepth = MAX_A2UI_DEPTH,
  maxExpansion = MAX_A2UI_EXPANSION
): { valid: boolean; error?: string } {
  const activePath = new Set<string>();
  const sizes = new Map<string, GraphSize>();

  function dfs(id: string, depth: number): { valid: boolean; error?: string; size?: GraphSize } {
    if (depth > maxDepth) {
      return {
        valid: false,
        error: `Component hierarchy exceeds maximum depth of ${maxDepth}`,
      };
    }
    if (sizes.has(id)) {
      const size = sizes.get(id)!;
      if (depth - 1 + size.depth > maxDepth) {
        return {
          valid: false,
          error: `Component hierarchy exceeds maximum depth of ${maxDepth}`,
        };
      }
      return { valid: true, size };
    }
    if (activePath.has(id)) {
      return {
        valid: false,
        error: `Cyclic reference detected at component "${id}"`,
      };
    }

    const comp = componentMap.get(id);
    if (!comp) {
      const leafSize: GraphSize = { depth: 1, expansion: 1 };
      sizes.set(id, leafSize);
      return { valid: true, size: leafSize };
    }

    activePath.add(id);
    const size: GraphSize = { depth: 1, expansion: 1 };

    const childIds: string[] = [];
    if ((comp.component === 'Card' || comp.component === 'Button') && comp.child) {
      childIds.push(comp.child);
    } else if (comp.component === 'Column' && Array.isArray(comp.children)) {
      for (const child of comp.children) {
        if (typeof child === 'string') {
          childIds.push(child);
        }
      }
    }

    for (const childId of childIds) {
      const res = dfs(childId, depth + 1);
      if (!res.valid) return res;
      if (res.size) {
        size.depth = Math.max(size.depth, res.size.depth + 1);
        size.expansion += res.size.expansion;
        if (size.expansion > maxExpansion) {
          return {
            valid: false,
            error: `Component hierarchy exceeds maximum expansion of ${maxExpansion}`,
          };
        }
      }
    }

    activePath.delete(id);
    sizes.set(id, size);
    return { valid: true, size };
  }

  const result = dfs(rootId, 1);
  return { valid: result.valid, error: result.error };
}
