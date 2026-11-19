import React, { useState, useId } from 'react';
import type {
  A2UIComponent,
  ButtonComponent,
  CardComponent,
  ChoiceOption,
  ChoicePickerComponent,
  ColumnComponent,
  TextComponent,
  TextFieldComponent,
} from './types';
import {
  dataKey,
  getComponents,
  resolveDynamicString,
  MAX_A2UI_DEPTH,
  MAX_A2UI_EXPANSION,
  validateComponentGraph,
} from './buildData';

export interface A2UIFormProps {
  payload: unknown;
  values: Record<string, unknown>;
  onValueChange: (key: string, value: unknown) => void;
  onAction: (actionName: string) => void;
  disabled?: boolean;
  scopeId?: string;
}

export const A2UIForm: React.FC<A2UIFormProps> = ({
  payload,
  values,
  onValueChange,
  onAction,
  disabled = false,
  scopeId,
}) => {
  const reactId = useId();
  const surfaceScopeId = scopeId ?? reactId;

  const components = getComponents(payload);
  const componentMap = new Map<string, A2UIComponent>();
  const buttonChildIds = new Set<string>();
  for (const c of components) {
    if (c?.id) {
      componentMap.set(c.id, c);
    }
    if (c?.component === 'Button' && c.child) {
      buttonChildIds.add(c.child);
    }
  }

  const root = componentMap.get('root') ?? components[0];
  if (!root) {
    return <div className="a2ui-empty">No components in surface</div>;
  }

  const validation = validateComponentGraph(root.id, componentMap);
  if (!validation.valid) {
    return (
      <div role="alert" className="a2ui-surface-error" data-testid="a2ui-surface-error">
        Unable to render surface: {validation.error}
      </div>
    );
  }

  const renderComponent = (
    id: string,
    activePath = new Set<string>(),
    depth = 1,
    visits = { count: 0 }
  ): React.ReactNode => {
    if (activePath.has(id)) {
      return (
        <div role="alert" className="a2ui-surface-error" data-testid="a2ui-surface-error">
          Cyclic reference detected
        </div>
      );
    }
    if (depth > MAX_A2UI_DEPTH) {
      return (
        <div role="alert" className="a2ui-surface-error" data-testid="a2ui-surface-error">
          Maximum component depth exceeded
        </div>
      );
    }
    if (visits.count >= MAX_A2UI_EXPANSION) {
      return (
        <div role="alert" className="a2ui-surface-error" data-testid="a2ui-surface-error">
          Maximum component expansion exceeded
        </div>
      );
    }
    visits.count += 1;

    const comp = componentMap.get(id);
    if (!comp) return null;

    const nextPath = new Set(activePath);
    nextPath.add(id);
    const nextDepth = depth + 1;

    switch (comp.component) {
      case 'Card':
        return renderCard(comp, nextPath, nextDepth, visits);
      case 'Column':
        return renderColumn(comp, nextPath, nextDepth, visits);
      case 'Text':
        return renderText(comp);
      case 'TextField':
        return renderTextField(comp);
      case 'ChoicePicker':
        return renderChoicePicker(comp);
      case 'Button':
        return renderButton(comp);
      default:
        return null;
    }
  };

  const renderCard = (
    comp: CardComponent,
    activePath: Set<string>,
    depth: number,
    visits: { count: number }
  ) => {
    return (
      <div key={comp.id} className="a2ui-card" data-testid={`a2ui-card-${comp.id}`}>
        {comp.child ? (
          visits.count >= MAX_A2UI_EXPANSION ? (
            <div role="alert" className="a2ui-surface-error" data-testid="a2ui-surface-error">
              Maximum component expansion exceeded
            </div>
          ) : (
            renderComponent(comp.child, activePath, depth, visits)
          )
        ) : null}
      </div>
    );
  };

  const renderColumn = (
    comp: ColumnComponent,
    activePath: Set<string>,
    depth: number,
    visits: { count: number }
  ) => {
    const justifyMap: Record<string, string> = {
      start: 'flex-start',
      center: 'center',
      end: 'flex-end',
      spaceBetween: 'space-between',
      spaceAround: 'space-around',
      spaceEvenly: 'space-evenly',
      stretch: 'stretch',
    };
    const alignMap: Record<string, string> = {
      start: 'flex-start',
      center: 'center',
      end: 'flex-end',
      stretch: 'stretch',
    };

    const style: React.CSSProperties = {
      display: 'flex',
      flexDirection: 'column',
      gap: '12px',
      justifyContent: comp.justify ? justifyMap[comp.justify] ?? comp.justify : undefined,
      alignItems: comp.align ? alignMap[comp.align] ?? comp.align : undefined,
    };

    const renderedChildren: React.ReactNode[] = [];
    if (Array.isArray(comp.children)) {
      for (const childId of comp.children) {
        if (buttonChildIds.has(childId)) continue;
        if (visits.count >= MAX_A2UI_EXPANSION) {
          renderedChildren.push(
            <div key={`overflow-${childId}`} role="alert" className="a2ui-surface-error" data-testid="a2ui-surface-error">
              Maximum component expansion exceeded
            </div>
          );
          break;
        }
        renderedChildren.push(
          <React.Fragment key={childId}>
            {renderComponent(childId, activePath, depth, visits)}
          </React.Fragment>
        );
      }
    }

    return (
      <div key={comp.id} className="a2ui-column" data-testid={`a2ui-column-${comp.id}`} style={style}>
        {renderedChildren}
      </div>
    );
  };

  const renderText = (comp: TextComponent) => {
    const text = resolveDynamicString(payload, comp.text, values) ?? '';
    const isCaption = comp.variant === 'caption';
    return isCaption ? (
      <span key={comp.id} className="a2ui-text a2ui-text-caption">
        {text}
      </span>
    ) : (
      <p key={comp.id} className="a2ui-text a2ui-text-body">
        {text}
      </p>
    );
  };

  const renderTextField = (comp: TextFieldComponent) => {
    const path = comp.value?.['@path'] ?? '';
    const key = dataKey(path);
    const label = resolveDynamicString(payload, comp.label, values) ?? key;
    const placeholder = resolveDynamicString(payload, comp.placeholder, values);
    const rawVal = values[key];
    const val = typeof rawVal === 'string' ? rawVal : (rawVal ?? '');
    const inputId = `${surfaceScopeId}-a2ui-input-${comp.id}`;

    return (
      <div key={comp.id} className="a2ui-field a2ui-text-field" data-testid={`a2ui-field-${key}`}>
        <label htmlFor={inputId} className="a2ui-label">
          {label}
        </label>
        {comp.variant === 'longText' ? (
          <textarea
            id={inputId}
            name={key}
            value={val as string}
            placeholder={placeholder}
            disabled={disabled}
            onChange={(e) => onValueChange(key, e.target.value)}
            className="a2ui-textarea"
          />
        ) : (
          <input
            id={inputId}
            type={
              comp.variant === 'number'
                ? 'number'
                : comp.variant === 'obscured'
                ? 'password'
                : 'text'
            }
            name={key}
            value={val as string}
            placeholder={placeholder}
            disabled={disabled}
            onChange={(e) => onValueChange(key, e.target.value)}
            className="a2ui-input"
          />
        )}
      </div>
    );
  };

  const renderChoicePicker = (comp: ChoicePickerComponent) => {
    return (
      <ChoicePickerItem
        key={comp.id}
        comp={comp}
        payload={payload}
        values={values}
        disabled={disabled}
        surfaceScopeId={surfaceScopeId}
        onValueChange={onValueChange}
      />
    );
  };

  const renderButton = (comp: ButtonComponent) => {
    const actionName = comp.action?.event?.name;
    const childComp = comp.child ? componentMap.get(comp.child) : undefined;
    let label: string | undefined;
    if (childComp && childComp.component === 'Text') {
      label = resolveDynamicString(payload, childComp.text, values);
    } else if (comp.child) {
      label = comp.child;
    }

    const harnxExt = (comp.metadata?.extensions as Record<string, unknown> | undefined)?.harnx as
      | Record<string, unknown>
      | undefined;
    const visibility =
      typeof harnxExt?.visibility === 'string' ? harnxExt.visibility : undefined;
    const buttonTitle = visibility ? `Visibility: ${visibility}` : undefined;

    const variantClass =
      comp.variant === 'primary'
        ? 'a2ui-btn-primary'
        : comp.variant === 'borderless'
        ? 'a2ui-btn-borderless'
        : 'a2ui-btn-default';

    return (
      <button
        key={comp.id}
        type="button"
        disabled={disabled}
        onClick={() => {
          if (actionName) {
            onAction(actionName);
          }
        }}
        className={`a2ui-btn ${variantClass} aui-interrupt-submit`}
        data-testid={`a2ui-btn-${actionName || comp.id}`}
        data-visibility={visibility}
        title={buttonTitle}
      >
        {label || actionName || 'Submit'}
      </button>
    );
  };

  return (
    <div className="a2ui-form-root" data-testid="a2ui-form-root">
      {renderComponent(root.id)}
    </div>
  );
};

interface ChoicePickerItemProps {
  comp: ChoicePickerComponent;
  payload: unknown;
  values: Record<string, unknown>;
  disabled: boolean;
  surfaceScopeId: string;
  onValueChange: (key: string, value: unknown) => void;
}

const ChoicePickerItem: React.FC<ChoicePickerItemProps> = ({
  comp,
  payload,
  values,
  disabled,
  surfaceScopeId,
  onValueChange,
}) => {
  const [filter, setFilter] = useState('');
  const path = comp.value?.['@path'] ?? '';
  const key = dataKey(path);
  const label = resolveDynamicString(payload, comp.label, values);
  const exclusive = comp.variant !== 'multipleSelection';
  const rawSelection = values[key];
  const selectedValues: string[] = Array.isArray(rawSelection)
    ? (rawSelection as string[])
    : typeof rawSelection === 'string' && rawSelection.length > 0
    ? [rawSelection]
    : [];

  const options: ChoiceOption[] = Array.isArray(comp.options) ? comp.options : [];
  const resolvedOptions = options.map((opt) => ({
    label: resolveDynamicString(payload, opt.label, values) ?? opt.value,
    value: opt.value,
  }));

  const visibleOptions = filter.trim()
    ? resolvedOptions.filter(
        (o) =>
          o.label.toLowerCase().includes(filter.toLowerCase()) ||
          o.value.toLowerCase().includes(filter.toLowerCase())
      )
    : resolvedOptions;

  const handleSelect = (optionValue: string) => {
    if (exclusive) {
      onValueChange(key, [optionValue]);
    } else {
      const isSelected = selectedValues.includes(optionValue);
      const next = isSelected
        ? selectedValues.filter((v) => v !== optionValue)
        : [...selectedValues, optionValue];
      onValueChange(key, next);
    }
  };

  return (
    <fieldset className="a2ui-field a2ui-choice-picker" data-testid={`a2ui-choice-${key}`}>
      {label && <legend className="a2ui-label">{label}</legend>}
      {comp.filterable && (
        <input
          type="text"
          placeholder="Filter options..."
          value={filter}
          disabled={disabled}
          onChange={(e) => setFilter(e.target.value)}
          className="a2ui-filter-input"
          aria-label={`Filter ${label ?? 'options'}`}
        />
      )}
      <div
        className={`a2ui-choice-options ${
          comp.displayStyle === 'chips' ? 'a2ui-choice-chips' : 'a2ui-choice-group'
        }`}
      >
        {visibleOptions.map((opt) => {
          const isChecked = selectedValues.includes(opt.value);
          const optionId = `${surfaceScopeId}-choice-${comp.id}-${opt.value}`;
          const radioGroupName = `${surfaceScopeId}-choice-${comp.id}`;
          return (
            <label
              key={opt.value}
              htmlFor={optionId}
              className={`a2ui-choice-option ${isChecked ? 'selected' : ''}`}
            >
              <input
                id={optionId}
                type={exclusive ? 'radio' : 'checkbox'}
                name={radioGroupName}
                value={opt.value}
                checked={isChecked}
                disabled={disabled}
                onChange={() => handleSelect(opt.value)}
              />
              <span>{opt.label}</span>
            </label>
          );
        })}
      </div>
    </fieldset>
  );
};
