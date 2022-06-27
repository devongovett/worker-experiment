const binding = require('./');
const {parentPort} = require('worker_threads');

console.log('start worker')
binding.registerWorker(value => {
  let x = [];
  for (let i = 0; i < 10000; i++) {
    x.push(i * value);
  }
  return x[x.length - 1];
});

parentPort.postMessage('ready');
