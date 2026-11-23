// swift-tools-version:5.9
// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import PackageDescription

let package = Package(
    name: "AtalantaSyncProbe",
    platforms: [.iOS(.v14)],
    products: [
        .library(
            name: "AtalantaSyncProbe",
            targets: ["AtalantaSyncProbe"]
        ),
    ],
    targets: [
        .target(
            name: "AtalantaSyncProbe",
            path: "Sources"
        ),
    ]
)
