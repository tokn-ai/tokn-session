import AppKit
import NaturalLanguage
import SwiftUI
@preconcurrency import Translation

private typealias Completion = @convention(c) (UInt64, UnsafePointer<CChar>) -> Void

private struct TranslationInput: Decodable {
  let texts: [String]
  let target_language: String
}

private struct TranslationOutput: Encodable {
  let texts: [String]?
  let error: String?
  let recovery: String?
}

private func complete(
  _ token: UInt64, _ callback: Completion,
  texts: [String]? = nil, error: String? = nil, recovery: String? = nil
) {
  let result = TranslationOutput(texts: texts, error: error, recovery: recovery)
  guard let data = try? JSONEncoder().encode(result), let json = String(data: data, encoding: .utf8) else {
    "{\"error\":\"Could not encode the translation result.\"}".withCString { callback(token, $0) }
    return
  }
  // Rust copies this JSON during the call; no Swift-owned pointer escapes.
  json.withCString { callback(token, $0) }
}

struct TranslationFailure {
  let error: String
  let recovery: String?
}

@available(macOS 15.0, *)
func translationFailure(for error: any Error) -> TranslationFailure {
  let description = error is CancellationError ? "Translation cancelled." : "Apple Translation: \(error.localizedDescription)"
  var details = [description]
  if let reason = (error as NSError).localizedFailureReason?.trimmingCharacters(in: .whitespacesAndNewlines),
     !reason.isEmpty, !description.localizedCaseInsensitiveContains(reason) {
    details.append(reason)
  }
  // Missing language models can surface as this public engine error, but it
  // also covers other failures. Offer settings without claiming a diagnosis.
  let recovery = TranslationError.internalError ~= error ? "translation_settings" : nil
  if recovery != nil {
    details.append("Check or download the source and target languages in macOS Translation settings, then retry.")
  }
  return TranslationFailure(error: details.joined(separator: "\n"), recovery: recovery)
}

@available(macOS 15.0, *)
private func makeRequests(texts: [String], indices: [Int]) -> sending [TranslationSession.Request] {
  indices.map { TranslationSession.Request(sourceText: texts[$0], clientIdentifier: String($0)) }
}

// Keep language detection independent of TranslationSession so it can be
// exercised without installed models or permission UI.
@available(macOS 15.0, *)
struct TranslationPlan: Sendable {
  let texts: [String]
  let input_indices: [Int]
  let input_count: Int
  let batches: [(language: String, indices: [Int])]

  func reassemble(_ translated: [String]) -> [String] {
    precondition(translated.count == texts.count)
    var result = Array(repeating: "", count: input_count)
    for (piece, input) in input_indices.enumerated() { result[input] += translated[piece] }
    return result
  }
}

@available(macOS 15.0, *)
func translationPlan(for inputs: [String]) -> TranslationPlan {
  func detect(_ text: String) -> (NLLanguage?, Double) {
    let recognizer = NLLanguageRecognizer()
    recognizer.processString(text)
    let language = recognizer.dominantLanguage
    return (language, language.flatMap { recognizer.languageHypotheses(withMaximum: 1)[$0] } ?? 0)
  }
  func letters(_ text: String) -> Int { text.unicodeScalars.filter { CharacterSet.letters.contains($0) }.count }
  func hasHan(_ text: String) -> Bool {
    text.unicodeScalars.contains { scalar in
      (0x3400...0x9fff).contains(scalar.value) || (0xf900...0xfaff).contains(scalar.value)
        || (0x20000...0x323af).contains(scalar.value)
    }
  }
  func trim(_ range: Range<String.Index>, in text: String) -> Range<String.Index> {
    var start = range.lowerBound
    var end = range.upperBound
    while start < end && text[start].isWhitespace { start = text.index(after: start) }
    while start < end && text[text.index(before: end)].isWhitespace { end = text.index(before: end) }
    return start..<end
  }

  let combined = inputs.joined(separator: " ")
  let (aggregate, confidence) = detect(combined)
  // Isolated Markdown nodes such as "in", "a", "CLI" and "Rust" are
  // unreliable language samples. Prefer the surrounding response's prose.
  let context = confidence >= 0.8 && letters(combined) >= 10 ? aggregate : .english
  let latin_runs = try! NSRegularExpression(pattern: "\\p{Latin}[\\p{Latin}\\p{M}\\p{N}\\p{P}\\p{Zs}\\t]*")
  var texts: [String] = []
  var input_indices: [Int] = []
  var batches: [(language: String, indices: [Int])] = []

  func append(_ value: String, input: Int, translate: Bool) {
    guard !value.isEmpty else { return }
    let index = texts.count
    texts.append(value)
    input_indices.append(input)
    guard translate, letters(value) > 0 else { return }
    let (detected, confidence) = detect(value)
    let words = value.split(whereSeparator: { $0.isWhitespace }).count
    let sufficiently_long = letters(value) >= 20 || (letters(value) >= 10 && words >= 2)
    let language: NLLanguage?
    if hasHan(value), detected == .simplifiedChinese {
      language = .simplifiedChinese
    } else if sufficiently_long && confidence >= 0.9 {
      language = detected
    } else {
      // A Latin fragment in mostly Chinese prose still needs translation; it
      // must not inherit Chinese and silently disappear from the work list.
      language = context == .simplifiedChinese && !hasHan(value) ? .english : context
    }
    if language == .simplifiedChinese { return }
    let key = language?.rawValue ?? "unknown"
    if let batch = batches.firstIndex(where: { $0.language == key && $0.indices.count < 128 }) {
      batches[batch].indices.append(index)
    } else {
      batches.append((language: key, indices: [index]))
    }
  }

  for (input, text) in inputs.enumerated() {
    if Task.isCancelled { break }
    let tokenizer = NLTokenizer(unit: .sentence)
    tokenizer.string = text
    var ranges: [Range<String.Index>] = []
    tokenizer.enumerateTokens(in: text.startIndex..<text.endIndex) { sentence, _ in
      // Also split a substantial Latin clause embedded in a Chinese sentence.
      // Short identifiers (API, CLI, names) stay with their surrounding prose.
      let sentence_text = String(text[sentence])
      var start = sentence.lowerBound
      if hasHan(sentence_text) {
        for match in latin_runs.matches(in: text, range: NSRange(sentence, in: text)) {
          guard let match_range = Range(match.range, in: text) else { continue }
          let candidate = trim(match_range, in: text)
          let value = String(text[candidate])
          guard letters(value) >= 10, value.split(whereSeparator: { $0.isWhitespace }).count >= 2 else { continue }
          if start < candidate.lowerBound { ranges.append(start..<candidate.lowerBound) }
          ranges.append(candidate)
          start = candidate.upperBound
        }
      }
      if start < sentence.upperBound { ranges.append(start..<sentence.upperBound) }
      return true
    }
    var cursor = text.startIndex
    for range in ranges {
      if Task.isCancelled { break }
      let content = trim(range, in: text)
      append(String(text[cursor..<content.lowerBound]), input: input, translate: false)
      append(String(text[content]), input: input, translate: true)
      cursor = content.upperBound
    }
    append(String(text[cursor...]), input: input, translate: false)
  }
  // Give Apple's automatic detector a substantial sample before short inline
  // fragments. Response identifiers restore the original presentation order.
  for batch in batches.indices {
    batches[batch].indices.sort {
      texts[$0].count == texts[$1].count ? $0 < $1 : texts[$0].count > texts[$1].count
    }
  }
  return TranslationPlan(texts: texts, input_indices: input_indices, input_count: inputs.count, batches: batches)
}

@available(macOS 15.0, *)
@MainActor
private final class TranslationJob {
  static var jobs: [UInt64: TranslationJob] = [:]

  let token: UInt64
  let input: TranslationInput
  let callback: Completion
  var host: NSView?
  var close_observer: NSObjectProtocol?
  var finished = false
  var started = false

  init(token: UInt64, input: TranslationInput, callback: @escaping Completion) {
    self.token = token
    self.input = input
    self.callback = callback
  }

  func attach(to window: NSWindow) {
    guard let content = window.contentView else {
      finish(error: "The viewer window is no longer available.")
      return
    }
    let hosting = TranslationHostingView(rootView: TranslationView(job: self))
    // Keep a real, visible view in this window's hierarchy: Translation uses it
    // to present Apple's language-download permission and progress UI. It must
    // not be hidden or moved to an offscreen/detached window.
    hosting.frame = NSRect(x: content.bounds.midX, y: content.bounds.midY, width: 1, height: 1)
    hosting.setAccessibilityHidden(true)
    host = hosting
    close_observer = NotificationCenter.default.addObserver(
      forName: NSWindow.willCloseNotification, object: window, queue: .main
    ) { [weak self] _ in
      MainActor.assumeIsolated { self?.finish(error: "Translation cancelled because the viewer window closed.") }
    }
    content.addSubview(hosting)
  }

  func translate(using session: TranslationSession) async {
    guard !finished, !started else { return }
    started = true
    do {
      try Task.checkCancellation()
      let source_texts = input.texts
      let planning = Task.detached(priority: .userInitiated) { translationPlan(for: source_texts) }
      let plan = await withTaskCancellationHandler {
        await planning.value
      } onCancel: {
        planning.cancel()
      }
      try Task.checkCancellation()
      guard !finished else { return }
      var output = plan.texts
      for (_, indices) in plan.batches {
        try Task.checkCancellation()
        guard !finished else { return }
        let expected = Set(indices)
        // Build fresh wire requests for each call and transfer them once. The
        // macOS 15 SDK's Request type does not yet declare Sendable.
        let requests = makeRequests(texts: plan.texts, indices: indices)
        let responses = try await session.translations(from: requests)
        try Task.checkCancellation()
        guard !finished else { return }
        guard responses.count == expected.count else {
          finish(error: "Apple Translation returned an incomplete response. Please retry.")
          return
        }
        var seen: Set<Int> = []
        for response in responses {
          guard let identifier = response.clientIdentifier,
                let index = Int(identifier), expected.contains(index),
                seen.insert(index).inserted else {
            finish(error: "Apple Translation returned an invalid response. Please retry.")
            return
          }
          output[index] = response.targetText
        }
        guard seen == expected else {
          finish(error: "Apple Translation returned an incomplete response. Please retry.")
          return
        }
      }
      finish(texts: plan.reassemble(output))
    } catch {
      let failure = translationFailure(for: error)
      finish(error: failure.error, recovery: failure.recovery)
    }
  }

  func finish(texts: [String]? = nil, error: String? = nil, recovery: String? = nil) {
    guard !finished else { return }
    finished = true
    complete(token, callback, texts: texts, error: error, recovery: recovery)
    if let observer = close_observer {
      NotificationCenter.default.removeObserver(observer)
      close_observer = nil
    }
    // Leave translationTask's action before removing its view. Cancellation
    // also removes the view, cancelling SwiftUI's task; no subsequent session
    // calls are made after the await above when a job has finished.
    DispatchQueue.main.async { [self] in
      host?.removeFromSuperview()
      host = nil
      Self.jobs.removeValue(forKey: token)
    }
  }
}

@available(macOS 15.0, *)
private struct TranslationView: View {
  let job: TranslationJob

  var body: some View {
    Color.clear
      .frame(width: 1, height: 1)
      .translationTask(source: nil, target: Locale.Language(identifier: job.input.target_language)) { session in
        await job.translate(using: session)
      }
  }
}

@available(macOS 15.0, *)
private final class TranslationHostingView: NSHostingView<TranslationView> {
  override func hitTest(_ point: NSPoint) -> NSView? { nil }
}

@_cdecl("tokn_translation_available")
public func translationAvailable() -> Bool {
  if #available(macOS 15.0, *) { return true }
  return false
}

// These entry points are invoked by Rust exclusively on the AppKit main
// thread. Only integer tokens cross the async boundary, never raw contexts.
@_cdecl("tokn_translation_start")
@MainActor
public func translationStart(
  _ window_pointer: UnsafeMutableRawPointer,
  _ token: UInt64,
  _ input_pointer: UnsafePointer<CChar>,
  _ callback: @escaping @convention(c) (UInt64, UnsafePointer<CChar>) -> Void
) {
  guard #available(macOS 15.0, *) else {
    complete(token, callback, error: "Apple Translation requires macOS 15 or later.")
    return
  }
  do {
    guard !TranslationJob.jobs.values.contains(where: { !$0.finished }) else {
      complete(token, callback, error: "Another response is being translated. Please retry shortly.")
      return
    }
    let data = Data(String(cString: input_pointer).utf8)
    let input = try JSONDecoder().decode(TranslationInput.self, from: data)
    let window = Unmanaged<NSWindow>.fromOpaque(window_pointer).takeUnretainedValue()
    let job = TranslationJob(token: token, input: input, callback: callback)
    TranslationJob.jobs[token] = job
    job.attach(to: window)
  } catch {
    complete(token, callback, error: "Could not read the translation request.")
  }
}

@_cdecl("tokn_translation_cancel")
@MainActor
public func translationCancel(_ token: UInt64) {
  if #available(macOS 15.0, *) {
    TranslationJob.jobs[token]?.finish(error: "Translation cancelled.")
  }
}
