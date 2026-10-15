export interface TurnChanges {
  files: string[];
  checked: boolean;
}

export function createTurnChanges() {
  const files = new Set<string>();
  let checked = true;
  return {
    edited(path: string) {
      files.add(path);
      checked = false;
    },
    ran() {
      checked = true;
    },
    take(): TurnChanges {
      const taken = { files: [...files], checked };
      files.clear();
      checked = true;
      return taken;
    },
  };
}
