## Throttling replication

###Status

Accepted

##Context

High churn causes lots of `PeerAdded` events, which triggers `interval_replication` to ensure new nodes have data. But even low levels of churn can cause a lot more messaging here than would be desireable.

##Decision
So we now limit the frequency with which we can trigger interval replication.

We do it at most once per minute.

##Consequences

Less replication, but a more measured pace.
