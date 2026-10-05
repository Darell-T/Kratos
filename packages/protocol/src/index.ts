import { z } from 'zod';

export const providers = ['codex', 'claude'] as const;
export const providerSchema = z.enum(providers);
export type Provider = z.infer<typeof providerSchema>;

export const workspaceSchema = z.object({
  id: z.string(),
  name: z.string(),
  path: z.string(),
  createdAt: z.string(),
});
export type Workspace = z.infer<typeof workspaceSchema>;

export const threadSchema = z.object({
  id: z.string(),
  workspaceId: z.string(),
  title: z.string(),
  provider: providerSchema,
  createdAt: z.string(),
  updatedAt: z.string(),
});
export type Thread = z.infer<typeof threadSchema>;

export const providerSessionSchema = z.object({
  id: z.string(),
  threadId: z.string(),
  provider: providerSchema,
  nativeId: z.string().nullable(),
  startedAt: z.string(),
  endedAt: z.string().nullable(),
});
export type ProviderSession = z.infer<typeof providerSessionSchema>;

export const threadEventBodySchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('user.message'), text: z.string() }),
]);
export type ThreadEventBody = z.infer<typeof threadEventBodySchema>;

export const threadEventSchema = z.object({
  threadId: z.string(),
  seq: z.number().int().positive(),
  at: z.string(),
  event: threadEventBodySchema,
});
export type ThreadEvent = z.infer<typeof threadEventSchema>;

const requiredText = z.string().trim().min(1);

export const createWorkspaceBody = z.object({ name: requiredText, path: requiredText });
export type CreateWorkspaceBody = z.infer<typeof createWorkspaceBody>;

export const createThreadBody = z.object({ workspaceId: requiredText, title: requiredText, provider: providerSchema });
export type CreateThreadBody = z.infer<typeof createThreadBody>;

export const postMessageBody = z.object({ text: requiredText });
export type PostMessageBody = z.infer<typeof postMessageBody>;

export const clientMessageSchema = z.discriminatedUnion('type', [
  z.object({ type: z.literal('subscribe'), threadId: z.string(), after: z.number().int().nonnegative() }),
  z.object({ type: z.literal('unsubscribe'), threadId: z.string() }),
]);
export type ClientMessage = z.infer<typeof clientMessageSchema>;

export const serverMessageSchema = z.discriminatedUnion('type', [
  z.object({ type: z.literal('event'), event: threadEventSchema }),
  z.object({ type: z.literal('error'), message: z.string() }),
]);
export type ServerMessage = z.infer<typeof serverMessageSchema>;
