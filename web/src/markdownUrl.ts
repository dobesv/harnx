import { defaultUrlTransform } from 'react-markdown';

export function markdownUrlTransform(value: string): string {
  return /^cid:(?:plan|media):/.test(value) ? value : defaultUrlTransform(value);
}
