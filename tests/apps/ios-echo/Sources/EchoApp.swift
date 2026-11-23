// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import SwiftUI

@main
struct AtalantaEchoApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}

struct ContentView: View {
    @State private var pairs: [(name: String, value: String)] = []

    var body: some View {
        List {
            Text("Atalanta Echo")
            Text("args=\(pairs.count)")
                .accessibilityIdentifier("args.count")
            if pairs.isEmpty {
                Text("no arguments")
            } else {
                ForEach(pairs, id: \.name) { pair in
                    Text("\(pair.name)=\(pair.value)")
                        .accessibilityIdentifier("arg.\(pair.name)")
                    Text("default.\(pair.name)=\(userDefault(for: pair.name))")
                        .accessibilityIdentifier("default.\(pair.name)")
                }
            }
        }
        .task {
            pairs = parseArguments()
        }
    }

    func parseArguments() -> [(name: String, value: String)] {
        let args = ProcessInfo.processInfo.arguments
        var result: [(name: String, value: String)] = []
        var i = 1
        while i < args.count {
            let arg = args[i]
            if arg.hasPrefix("-") && i + 1 < args.count {
                let name = String(arg.dropFirst())
                result.append((name: name, value: args[i + 1]))
                i += 2
            } else {
                i += 1
            }
        }
        result.sort { $0.name < $1.name }
        return result
    }

    func userDefault(for name: String) -> String {
        UserDefaults.standard.string(forKey: name) ?? "nil"
    }
}
