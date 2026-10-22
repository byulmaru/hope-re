const { ActivityCancellationType, proxyActivities } = require("@temporalio/workflow");

const { prepareHopeUpload } = proxyActivities({
  taskQueue: "media-hope",
  startToCloseTimeout: "30 minutes",
  scheduleToCloseTimeout: "35 minutes",
  heartbeatTimeout: "30 seconds",
  cancellationType: ActivityCancellationType.WAIT_CANCELLATION_COMPLETED,
  retry: { maximumAttempts: 1 },
});

exports.checkHope = async (input) => {
  await prepareHopeUpload(input);
};
