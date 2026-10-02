import OfficeCore
import SwiftUI

struct ConversationWorkProgressView: View {
    @ObservedObject var store: ConversationWorkProgressStore
    let characterName: String

    var body: some View {
        if let progress = store.progress {
            ConversationWorkProgressCard(progress: progress, characterName: characterName)
                .id(progress.turnID)
        }
    }
}

struct ConversationWorkProgressCard: View {
    let progress: ConversationWorkProgress
    let characterName: String
    @State private var showsDetails = false

    private var accent: Color { DashboardPalette.providerAccent(for: progress.backend) }
    private var statusColor: Color {
        switch progress.state {
        case .failed: .red
        case .needsInput, .operationError: .orange
        case .interrupted, .waiting: .secondary
        default: accent
        }
    }
    private var icon: String {
        switch progress.state {
        case .running: "circle.dotted"
        case .waiting: "clock"
        case .needsInput: "person.crop.circle.badge.questionmark"
        case .completed: "checkmark.circle"
        case .failed: "exclamationmark.circle"
        case .operationError: "exclamationmark.triangle"
        case .interrupted: "pause.circle"
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: icon)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(statusColor)
                .padding(.top, 2)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 7) {
                    Text(characterName).font(.system(size: 11, weight: .medium)).foregroundStyle(.secondary)
                    Text(OfficeLocalization.string(progress.headline))
                        .font(.system(size: 12, weight: .semibold))
                        .lineLimit(1).truncationMode(.tail)
                }
                if let report = progress.report {
                    Text(OfficeLocalization.string("진행 설명") + " · " + report)
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                        .lineLimit(1).truncationMode(.tail)
                } else if let result = progress.latestResult {
                    Text(eventText(result)).font(.system(size: 11)).foregroundStyle(.secondary).lineLimit(1)
                } else {
                    Text(OfficeLocalization.string("새 작업 기록이 도착하면 여기에 표시합니다"))
                        .font(.system(size: 11)).foregroundStyle(.secondary).lineLimit(1)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            Button { showsDetails.toggle() } label: {
                Image(systemName: "list.bullet.rectangle")
                    .font(.system(size: 14, weight: .medium))
                    .foregroundStyle(accent)
                    .frame(width: 28, height: 28)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(OfficeLocalization.string("작업 현황 자세히 보기"))
            .accessibilityLabel(OfficeLocalization.string("작업 현황 자세히 보기"))
            .popover(isPresented: $showsDetails, arrowEdge: .bottom) { details }
        }
        .padding(.horizontal, 14).padding(.vertical, 9)
        .officeGameSurface(accent: accent, emphasis: .conversation, cornerRadius: 0)
        .overlay(alignment: .bottom) { Divider().opacity(0.4) }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("conversationWorkProgress")
    }

    private func eventText(_ event: ConversationWorkProgress.Event) -> String {
        let text = event.status == .running ? event.phase.runningText : event.phase.finishedText
        return event.status == .failed
            ? OfficeLocalization.string("오류 기록") + " · " + OfficeLocalization.string(text)
            : OfficeLocalization.string(text)
    }

    private var details: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(OfficeLocalization.string("작업 현황")).font(.headline)
                Spacer()
                Text(characterName).font(.subheadline).foregroundStyle(accent)
            }
            Text(OfficeLocalization.string(progress.headline)).font(.system(size: 13, weight: .semibold)).foregroundStyle(statusColor)
            if let report = progress.report {
                Text(OfficeLocalization.string("직원이 남긴 진행 설명")).font(.caption).foregroundStyle(.secondary)
                Text(report).font(.system(size: 12)).fixedSize(horizontal: false, vertical: true)
            }
            if let next = progress.nextStep {
                Text(OfficeLocalization.string("계획에 적힌 다음 작업") + " · " + next)
                    .font(.system(size: 12)).lineLimit(3)
            }
            if !progress.history.isEmpty {
                Divider()
                ForEach(progress.history) { event in
                    HStack(alignment: .top, spacing: 8) {
                        Image(systemName: event.status == .failed ? "exclamationmark.circle" : event.status == .running ? "circle.dotted" : "checkmark")
                            .foregroundStyle(event.status == .failed ? Color.red : accent)
                        Text(eventText(event)).frame(maxWidth: .infinity, alignment: .leading)
                        Text(OfficeLocalization.date(event.at, dateStyle: .omitted, time: .shortened)).foregroundStyle(.secondary)
                    }.font(.system(size: 11))
                }
            }
            Text(OfficeLocalization.string("실제 작업 기록 기준입니다. 명령 종료가 업무 성공을 뜻하지는 않습니다."))
                .font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            Text(OfficeLocalization.string("마지막 기록") + " · " + OfficeLocalization.date(progress.recordedAt, dateStyle: .abbreviated, time: .shortened))
                .font(.system(size: 10)).foregroundStyle(.secondary)
        }
        .padding(18).frame(width: 360)
        .officeGameSurface(accent: accent, emphasis: .conversation, cornerRadius: 12)
        .environment(\.officeHoverEffectsEnabled, false)
    }
}
