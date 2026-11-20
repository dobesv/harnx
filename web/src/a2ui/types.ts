export type TextVariant = 'body' | 'caption';
export type ButtonVariant = 'default' | 'primary' | 'borderless';
export type TextFieldVariant = 'shortText' | 'longText' | 'number' | 'obscured';
export type ChoiceVariant = 'mutuallyExclusive' | 'multipleSelection';
export type ChoiceDisplayStyle = 'checkbox' | 'chips';

export interface DataBinding {
  '@path': string;
}

export type DynamicString = string | DataBinding;

export interface ChoiceOption {
  label: DynamicString;
  value: string;
}

export interface ButtonActionDef {
  event: {
    name: string;
    userMessage?: DynamicString;
    context?: Record<string, unknown>;
  };
}

export interface CardComponent {
  id: string;
  component: 'Card';
  child: string;
}

export interface ColumnComponent {
  id: string;
  component: 'Column';
  children: string[];
  justify?: 'start' | 'center' | 'end' | 'spaceBetween' | 'spaceAround' | 'spaceEvenly' | 'stretch';
  align?: 'center' | 'end' | 'start' | 'stretch';
}

export interface TextComponent {
  id: string;
  component: 'Text';
  text: DynamicString;
  variant?: TextVariant;
}

export interface ButtonComponent {
  id: string;
  component: 'Button';
  child: string;
  variant?: ButtonVariant;
  action: ButtonActionDef;
  metadata?: Record<string, unknown>;
}

export interface TextFieldComponent {
  id: string;
  component: 'TextField';
  label: DynamicString;
  value: DataBinding;
  placeholder?: DynamicString;
  variant?: TextFieldVariant;
}

export interface ChoicePickerComponent {
  id: string;
  component: 'ChoicePicker';
  label?: DynamicString;
  value: DataBinding;
  options: ChoiceOption[];
  variant?: ChoiceVariant;
  displayStyle?: ChoiceDisplayStyle;
  filterable?: boolean;
}

export type A2UIComponent =
  | CardComponent
  | ColumnComponent
  | TextComponent
  | ButtonComponent
  | TextFieldComponent
  | ChoicePickerComponent;

export interface A2UICreateSurface {
  surfaceId?: string;
  catalogId?: string;
  sendDataModel?: boolean;
  dataModel?: Record<string, unknown>;
  components: A2UIComponent[];
  metadata?: Record<string, unknown>;
}

export interface A2UISurfaceEnvelope {
  version: '1.0';
  createSurface: A2UICreateSurface;
}

export interface InputFieldDescriptor {
  componentId: string;
  path: string;
  key: string;
  kind: 'TextField' | 'ChoicePicker';
  label?: string;
  options: Array<{ label: string; value: string }>;
  exclusive: boolean;
}

export interface ButtonActionDescriptor {
  componentId: string;
  name: string;
  label?: string;
  context: Record<string, unknown>;
}

export interface A2uiSurfaceResolution {
  action: string;
  visibility?: string;
  noteText?: string;
}

export interface A2uiSurfaceEvent {
  surfaceId: string;
  publishSeq: number;
  toolCallId?: string;
  payload?: unknown;
  action?: A2uiSurfaceResolution | null;
}

export interface A2uiSurface {
  surfaceId: string;
  publishSeq: number;
  toolCallId?: string;
  payload?: unknown;
  action: A2uiSurfaceResolution | null;
}
