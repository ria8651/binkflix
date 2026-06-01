// Synchronous pre-stub for window.binkflixPlayer.
//
// Loaded before player.js (an ES module, which loads async). Queues any
// calls made during the gap; the module replays the queue once it's ready.
window.binkflixPlayer = {
    __queue: [],
    setAss:       function () { this.__queue.push(["setAss",       [...arguments]]); return Promise.resolve(); },
    setVtt:       function () { this.__queue.push(["setVtt",       [...arguments]]); return Promise.resolve(); },
    clear:        function () { this.__queue.push(["clear",        [...arguments]]); return Promise.resolve(); },
    attach:       function () { this.__queue.push(["attach",       [...arguments]]); return Promise.resolve(); },
    detach:       function () { this.__queue.push(["detach",       [...arguments]]); return Promise.resolve(); },
    initControls: function () { this.__queue.push(["initControls", [...arguments]]); return Promise.resolve(); },
};
