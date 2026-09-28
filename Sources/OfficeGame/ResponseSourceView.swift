// 이 파일은 응답이 근거로 사용한 출처를 종류별로 한눈에 표시한다.

import OfficeCore
import SwiftUI

enum ResponseSourceDisplayPolicy {
    static func showsSources(
        hasSources: Bool,
        isRunning: Bool,
        animatesResponse: Bool
    ) -> Bool {
        hasSources && !isRunning && !animatesResponse
    }
}

struct ResponseSourceWarningView: View {
    let message: String
    var accessibilityIdentifier: String?

    var body: some View {
        HStack(alignment: .top, spacing: 7) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .font(.system(size: 11, weight: .semibold))

            Text(OfficeLocalization.systemMessage(message))
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            Color.orange.opacity(0.08),
            in: RoundedRectangle(cornerRadius: 8, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .stroke(Color.orange.opacity(0.18))
        }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier(
            accessibilityIdentifier ?? "responseSourceWarning"
        )
    }
}

struct ResponseSourceList: View {
    let sources: [LiveFeedSource]
    let workspaceDirectory: String
    @State private var showsAllSources = false

    private static let collapsedLimit = 3

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Label(
                OfficeLocalization.string("응답 근거"),
                systemImage: "link"
            )
            .font(.system(size: 10.5, weight: .bold))
            .foregroundStyle(.secondary)

            ForEach(visibleSources) { source in
                ResponseSourceRow(
                    source: source,
                    workspaceDirectory: workspaceDirectory
                )
            }

            if sources.count > Self.collapsedLimit {
                Button {
                    showsAllSources.toggle()
                } label: {
                    Label(
                        showsAllSources
                            ? OfficeLocalization.string("출처 접기")
                            : OfficeLocalization.format(
                                "출처 %d개 더 보기",
                                sources.count - Self.collapsedLimit
                            ),
                        systemImage:
                            showsAllSources ? "chevron.up" : "chevron.down"
                    )
                    .font(.system(size: 9.5, weight: .semibold))
                }
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
            }
        }
        .padding(9)
        .background(
            Color.primary.opacity(0.025),
            in: RoundedRectangle(cornerRadius: 9, style: .continuous)
        )
        .accessibilityIdentifier("responseSources")
    }

    private var visibleSources: ArraySlice<LiveFeedSource> {
        sources.prefix(
            showsAllSources ? sources.count : Self.collapsedLimit
        )
    }
}

private struct ResponseSourceRow: View {
    let source: LiveFeedSource
    let workspaceDirectory: String

    var body: some View {
        HStack(alignment: .top, spacing: 7) {
            Text(OfficeLocalization.string(source.sourceKind.title))
                .font(.system(size: 8.5, weight: .bold))
                .foregroundStyle(sourceColor)
                .padding(.horizontal, 6)
                .frame(height: 18)
                .background(sourceColor.opacity(0.10), in: Capsule())

            VStack(alignment: .leading, spacing: 3) {
                Text(source.title)
                    .font(.system(size: 10.5, weight: .semibold))

                if source.sourceKind == .file {
                    WorkspaceFileRevealButton(
                        title: source.locator,
                        path: source.filePath,
                        workspaceDirectory: workspaceDirectory,
                        foregroundColor: .secondary,
                        accessibilityIdentifier: "responseSource-\(source.id)"
                    )
                } else if let webURL = source.webURL {
                    Link(destination: webURL) {
                        sourceLocatorText
                    }
                    .buttonStyle(.plain)
                    .accessibilityIdentifier("responseSource-\(source.id)")
                } else {
                    sourceLocatorText
                }

                if let excerpt = source.excerpt, !excerpt.isEmpty {
                    Text(excerpt)
                        .font(.system(size: 9.5))
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private var sourceColor: Color {
        switch source.sourceKind {
        case .rag:
            .purple
        case .database:
            .blue
        case .file:
            .green
        case .web:
            .cyan
        case .tool:
            .orange
        case .skill:
            .pink
        }
    }

    private var sourceLocatorText: some View {
        Text(source.locator)
            .font(.system(size: 9.5, design: .monospaced))
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .truncationMode(.middle)
    }
}
