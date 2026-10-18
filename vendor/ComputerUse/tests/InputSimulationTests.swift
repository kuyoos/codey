import CoreGraphics
import Foundation

@main
enum InputSimulationTests {
    static func expectFailure(_ operation: () throws -> Void) {
        do {
            try operation()
            fatalError("Expected event creation to fail")
        } catch {}
    }

    static func main() throws {
        let start = CGPoint(x: 10, y: 20)
        let end = CGPoint(x: 50, y: 60)
        let pid: pid_t = 42
        // Every test replaces both posting and sleeping; no event reaches macOS.
        for targeted in [false, true] {
            var posted: [CGEvent] = []
            var delivery = InputEventDelivery()
            delivery.post = { event, destination in
                precondition(destination == (targeted ? pid : nil))
                posted.append(event)
            }
            delivery.pause = { _ in }
            let create = delivery.mouseEvent
            delivery.mouseEvent = { source, type, point, button in
                type == .leftMouseUp ? nil : create(source, type, point, button)
            }
            expectFailure {
                if targeted {
                    try InputSimulation.clickTargeted(at: start, button: .left, clickCount: 1, pid: pid, delivery: delivery)
                } else {
                    try InputSimulation.clickGlobally(at: start, button: .left, clickCount: 1, delivery: delivery)
                }
            }
            precondition(posted.map(\.type) == [.mouseMoved], "A missing release must prevent mouse down")

            posted.removeAll()
            delivery.mouseEvent = create
            if targeted {
                try InputSimulation.clickTargeted(at: start, button: .left, clickCount: 2, pid: pid, delivery: delivery)
            } else {
                try InputSimulation.clickGlobally(at: start, button: .left, clickCount: 2, delivery: delivery)
            }
            precondition(posted.map(\.type) == [.mouseMoved, .leftMouseDown, .leftMouseUp, .mouseMoved, .leftMouseDown, .leftMouseUp])
            precondition(posted.allSatisfy { $0.getIntegerValueField(.mouseEventClickState) == 2 })

            posted.removeAll()
            var dragged = 0
            delivery.mouseEvent = { source, type, point, button in
                if type == .leftMouseDragged {
                    dragged += 1
                    if dragged == 2 { return nil }
                }
                return create(source, type, point, button)
            }
            expectFailure {
                if targeted {
                    try InputSimulation.dragTargeted(from: start, to: end, pid: pid, delivery: delivery)
                } else {
                    try InputSimulation.dragGlobally(from: start, to: end, delivery: delivery)
                }
            }
            precondition(posted.map(\.type) == [.mouseMoved, .leftMouseDown, .leftMouseDragged, .leftMouseUp])
            precondition(posted.last!.location == posted[2].location, "Failure must release at the last delivered point")

            posted.removeAll()
            delivery.mouseEvent = create
            if targeted {
                try InputSimulation.dragTargeted(from: start, to: end, pid: pid, delivery: delivery)
            } else {
                try InputSimulation.dragGlobally(from: start, to: end, delivery: delivery)
            }
            precondition(posted.first!.type == .mouseMoved && posted[1].type == .leftMouseDown)
            precondition(posted.dropFirst(2).dropLast().allSatisfy { $0.type == .leftMouseDragged })
            precondition(posted.last!.type == .leftMouseUp && posted.last!.location == end)
        }

        // Two modifier downs, key down/up and two modifier ups: fail each allocation.
        for failureIndex in 0..<6 {
            var delivery = InputEventDelivery()
            let create = delivery.keyboardEvent
            var allocations = 0
            var posted = 0
            delivery.keyboardEvent = { code, down in
                defer { allocations += 1 }
                return allocations == failureIndex ? nil : create(code, down)
            }
            delivery.post = { _, _ in posted += 1 }
            delivery.pause = { _ in }
            expectFailure { try InputSimulation.pressKey("cmd+shift+a", pid: pid, delivery: delivery) }
            precondition(posted == 0, "A failed keyboard sequence must not leave modifiers pressed")
        }
        var delivery = InputEventDelivery()
        var keys: [CGEvent] = []
        delivery.post = { event, destination in
            precondition(destination == pid)
            keys.append(event)
        }
        delivery.pause = { _ in }
        try InputSimulation.pressKey("cmd+shift+a", pid: pid, delivery: delivery)
        precondition(keys.map(\.type) == [.flagsChanged, .flagsChanged, .keyDown, .keyUp, .flagsChanged, .flagsChanged])
        let parsed = try KeyPressParser.parse("cmd+shift+a")
        precondition(keys.map { $0.getIntegerValueField(.keyboardEventKeycode) } == [
            Int64(parsed.modifiers[0].keyCode), Int64(parsed.modifiers[1].keyCode),
            Int64(parsed.keyCode), Int64(parsed.keyCode),
            Int64(parsed.modifiers[1].keyCode), Int64(parsed.modifiers[0].keyCode),
        ])
        precondition(keys[0].flags == .maskCommand)
        precondition(keys[1...4].allSatisfy { $0.flags == [.maskCommand, .maskShift] })
        precondition(keys[5].flags == .maskCommand)
        print("InputSimulation failure and event-order tests passed")
    }
}
