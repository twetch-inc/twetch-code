import { contextBridge, ipcRenderer } from "electron";

contextBridge.exposeInMainWorld("twetchWallet", {
  status: () => ipcRenderer.invoke("twetch:wallet-status"),
  api: (endpoint: string, body?: unknown) =>
    ipcRenderer.invoke("twetch:wallet-api", endpoint, body),
});
