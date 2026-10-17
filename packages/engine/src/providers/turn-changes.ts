export interface TurnChanges {
  files: string[];
  editedSinceCommand: boolean;
  ranCommand: boolean;
}

export function createTurnChanges() {
  const files = new Set<string>();
  let editedSinceCommand = false;
  let ranCommand = false;
  return {
    edited(path: string) {
      files.add(path);
      editedSinceCommand = true;
    },
    ran() {
      editedSinceCommand = false;
      ranCommand = true;
    },
    take(): TurnChanges {
      const taken = { files: [...files], editedSinceCommand, ranCommand };
      files.clear();
      editedSinceCommand = false;
      ranCommand = false;
      return taken;
    },
  };
}
