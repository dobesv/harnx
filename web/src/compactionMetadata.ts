export interface CompactionMetadata {
  message_id: string;
  compaction_seq: number;
  summary: string;
  from_seq: number;
  to_seq: number;
  kept_count?: number;
  earlier_compaction?: number;
}

type RequiredMetadata = Omit<CompactionMetadata, 'kept_count' | 'earlier_compaction'>;

function isSafeInteger(value: unknown): value is number {
  return Number.isSafeInteger(value);
}

const requiredFields = {
  message_id: (value: unknown): value is string =>
    typeof value === 'string' && value.trim().length > 0,
  compaction_seq: (value: unknown): value is number => isSafeInteger(value) && value >= 0,
  summary: (value: unknown): value is string => typeof value === 'string',
  from_seq: isSafeInteger,
  to_seq: isSafeInteger,
} satisfies { [K in keyof RequiredMetadata]: (value: unknown) => value is RequiredMetadata[K] };

function hasRequiredFields(
  record: Record<string, unknown>,
): record is RequiredMetadata & Record<string, unknown> {
  return Object.entries(requiredFields).every(([key, validate]) => validate(record[key]));
}

export function parseCompactionMetadata(value: unknown): CompactionMetadata | null {
  if (value === null || typeof value !== 'object') return null;
  const record = value as Record<string, unknown>;
  if (!hasRequiredFields(record)) return null;

  const metadata: CompactionMetadata = {
    message_id: record.message_id,
    compaction_seq: record.compaction_seq,
    summary: record.summary,
    from_seq: record.from_seq,
    to_seq: record.to_seq,
  };
  for (const key of ['kept_count', 'earlier_compaction'] as const) {
    if (isSafeInteger(record[key])) metadata[key] = record[key];
  }
  return metadata;
}
