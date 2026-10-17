export interface TurnChanges {
  files: string[];
  editedSinceCommand: boolean;
}

export function createTurnChanges() {
  const files = new Set<string>();
  let editedSinceCommand = false;
  return {
    edited(path: string) {
      files.add(path);
      editedSinceCommand = true;
    },
    ran() {
      editedSinceCommand = false;
    },
    take(): TurnChanges {
      const taken = { files: [...files], editedSinceCommand };
      files.clear();
      editedSinceCommand = false;
      return taken;
    },
  };
}
