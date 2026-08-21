// Launches `strand lsp` and hands the connection to VS Code.
//
// There is deliberately nothing else here: every answer comes from the server,
// so this file only has to find the binary and stay out of the way.

import { workspace, window, type ExtensionContext } from "vscode";
import {
  LanguageClient,
  TransportKind,
  type LanguageClientOptions,
  type ServerOptions,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

/** Where to find the `strand` binary. */
function serverCommand(): string {
  const configured = workspace.getConfiguration("strand").get<string>("server.path");
  // Falls back to whatever is on PATH, which is the normal case once the
  // toolchain is installed.
  if (!configured || configured.trim().length === 0) {
    return "strand";
  }

  // VS Code only expands `${workspaceFolder}` in launch and task files, not in
  // arbitrary settings, so a checked-in project setting has to be expanded
  // here. That is what lets this repo point at its own `target/debug` build.
  const folder = workspace.workspaceFolders?.[0]?.uri.fsPath;
  return configured
    .trim()
    .replace(/\$\{workspaceFolder\}/g, folder ?? "");
}

export async function activate(context: ExtensionContext): Promise<void> {
  const command = serverCommand();

  const server: ServerOptions = {
    run: { command, args: ["lsp"], transport: TransportKind.stdio },
    debug: { command, args: ["lsp"], transport: TransportKind.stdio },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "strand" }],
    // Surfaces a failed launch in a place people actually look.
    outputChannelName: "Strand Language Server",
  };

  client = new LanguageClient("strand", "Strand Language Server", server, clientOptions);

  try {
    await client.start();
  } catch (error) {
    window.showErrorMessage(
      `Could not start the Strand language server using \`${command} lsp\`. ` +
        `Set \`strand.server.path\` to the built binary. (${error})`,
    );
    return;
  }

  context.subscriptions.push({ dispose: () => void client?.stop() });
}

export async function deactivate(): Promise<void> {
  await client?.stop();
}
